// 挂件本体的三个自绘视图：拖动条、滚动数字、侧边径向表盘。
//
// 从 AppMain.swift 拆出来，逻辑未改动。

import AppKit
import QuartzCore

final class DragThroughView: NSView {
    // Dragging is AppKit's own window drag, started from `mouseDown` with
    // `NSWindow.performDrag(with:)` — the same path a title bar uses, with the
    // window moved by the system rather than by this process. That distinction
    // is the whole performance story: an earlier version repositioned the
    // window by hand on every mouse-drag event, and each `setFrameOrigin` is
    // ~1.2 ms of window-server machinery (child-window movement propagation,
    // a WindowManagement XPC round trip, vibrancy re-composite) fired 60-120
    // times a second off the main thread that is also pumping events. The
    // panel visibly lagged the pointer and the HUD's own animation stuttered.
    //
    // `performDrag(with:)` returns as soon as the drag *starts*, so the drag
    // events keep arriving here; this class only watches them, to keep telling
    // a drag (window moved → remember where it landed) apart from a click
    // (window did not move → open the dashboard).
    override var mouseDownCanMoveWindow: Bool { false }
    // right-click HUD → full menu (fallback when the status item is hidden)
    var menuHandler: (() -> NSMenu)?
    // plain click anywhere on the panel (labels / number / glass all route here)
    var clickHandler: (() -> Void)?
    /// Raised on mouse-down, before the drag has moved anything — the owner
    /// uses it to stop re-anchoring the panel for the duration of the gesture.
    var dragBeganHandler: (() -> Void)?
    /// Raised when the gesture ends (either way), so the owner can clear the
    /// "user is dragging" state.
    var dragSettledHandler: (() -> Void)?
    /// Raised when a gesture actually moved the panel, with the final origin.
    var dragMovedHandler: ((NSPoint) -> Void)?
    private var windowAt = NSPoint.zero
    private var dragged = false

    /// Buttons keep their own hit targets; every other subview (glass, labels,
    /// the rolling number) routes to this view. Without this, clicks land on
    /// leaf views that don't forward the event and the panel feels dead.
    override func hitTest(_ point: NSPoint) -> NSView? {
        if let hit = super.hitTest(point) {
            var node: NSView? = hit
            while let cur = node {
                // real controls keep their own clicks (buttons, docked bubble)
                if cur is NSButton || cur is RadialHUDView { return hit }
                node = cur.superview
            }
        }
        return self
    }

    override func mouseDown(with e: NSEvent) {
        windowAt = window?.frame.origin ?? .zero
        dragged = false
        dragBeganHandler?()
        // starts the drag; returns immediately, the window follows the pointer
        window?.performDrag(with: e)
    }

    override func mouseDragged(with e: NSEvent) {
        // the system is doing the moving — this only notices that it did
        if !dragged, let w = window, w.frame.origin != windowAt { dragged = true }
    }

    override func mouseUp(with e: NSEvent) { settle() }

    override func menu(for event: NSEvent) -> NSMenu? { menuHandler?() ?? super.menu(for: event) }

    /// End the gesture. Called from `mouseUp`, and again from the 1 s poll if a
    /// mouse-up ever goes missing, so the "user is dragging" state can never
    /// stick and freeze the panel in place.
    func settle() {
        guard let w = window else { return }
        if w.frame.origin != windowAt { dragged = true }
        if dragged {
            // Keep it inside the union of the real screens: a panel left on a
            // display that is no longer attached would be unreachable (the app
            // has a Dock icon now, but no window list to fetch it from).
            w.setFrameOrigin(DragThroughView.clampToVisibleUnion(w.frame.origin, size: w.frame.size))
            dragMovedHandler?(w.frame.origin)
        } else {
            clickHandler?()
        }
        dragSettledHandler?()
    }

    /// Keep a window inside the union of the real screens.
    ///
    /// A panel dragged (or restored from a saved position) onto a display that
    /// is no longer attached would be unreachable — there is nothing left to
    /// drag it back with and no window list to find it in. Clamping to the
    /// union is the cheapest way to make that impossible.
    /// Same, but inside the *usable* area of every screen.
    ///
    /// `frame` includes the menu bar and the Dock, and a HUD parked there is not
    /// merely ugly — it is unreachable, because the Dock draws over it and takes
    /// the clicks (measured on this machine: the panel's ⇥ button sat under the
    /// right-side Dock; STATUS.md 60).
    static func clampToVisibleUnion(_ origin: NSPoint, size: NSSize) -> NSPoint {
        let frames = NSScreen.screens.map { $0.visibleFrame }
        return clamp(origin, size: size, frames: frames)
    }

    private static func clamp(_ origin: NSPoint, size: NSSize, frames: [NSRect]) -> NSPoint {
        guard let minX = frames.map(\.minX).min(),
              let maxX = frames.map(\.maxX).max(),
              let minY = frames.map(\.minY).min(),
              let maxY = frames.map(\.maxY).max() else { return origin }
        return NSPoint(x: min(max(origin.x, minX), max(minX, maxX - size.width)),
                       y: min(max(origin.y, minY), max(minY, maxY - size.height)))
    }
}

/// Ceiling for one record's token fields, mirroring the server's
/// `MAX_RECORD_TOKENS`. Measured on this machine: the largest single record is
/// 3,126,357 tokens and the largest whole day is 1,176,446,631, so 10^11 is far
/// above anything an agent produces — it exists to tame a *corrupt* value
/// (a hand-edited transcript, a field that means bytes), see STATUS.md 58.
let maxRecordTokens = 100_000_000_000

/// Saturating addition for the aggregate path.
///
/// Swift traps on `Int` overflow instead of wrapping, and these numbers come off
/// the socket — the earlier failure mode was the app dying outright inside
/// `aggregate()` (`EXC_BREAKPOINT`, measured with two 5·10^18 records: the
/// server wrapped them to −8.4·10^18 and the client trapped adding them up).
@inline(__always) func satAdd(_ a: Int, _ b: Int) -> Int {
    let (r, over) = a.addingReportingOverflow(b)
    guard over else { return r }
    return b > 0 ? .max : .min
}

/// Mechanical-odometer number display: each digit is an independent column that
/// rolls to its new value; separators are static columns.
///
/// Every column is a solid-colour `CALayer` masked by a glyph image that is
/// rasterised once. That is what makes a roll cheap enough to run at 60 Hz:
///
///  * the previous form was ten `NSTextField`s per digit inside an `NSView`
///    strip, and moving that strip by one frame ran the real `NSView.setFrame:`
///    path — which mirrors the frame change into the Auto Layout engine
///    (`NSViewUpdateConstraintsForFrameChange` → `NSLayoutConstraint.setConstant:`
///    → `setNeedsLayout` on the window) and re-laid out the whole HUD. Plain
///    `CALayer` geometry never touches Auto Layout.
///  * the strip is rendered once, so a frame costs no text layout and no font
///    descriptor lookup. Reading `lineHeight` used to rebuild an
///    `NSAttributedString` and re-resolve `monospacedDigitSystemFont` on every
///    column of every frame; it measured as the single largest cost inside
///    `setText`, and `textColor` re-walked every label in the view.
///  * colour lives on the column layer, so the HUD's per-frame heat colour is a
///    handful of `backgroundColor` writes instead of invalidating ~100 text
///    layers (each of which then re-displayed through `NSTextLayer display` on
///    the next Core Animation commit).
///
/// The roll stays frame-driven rather than handed to Core Animation: an
/// interrupted `NSAnimationContext` restarts from the previous *model* position,
/// so under a stream of live updates the digits snapped backwards and never
/// settled. It is driven at 60 Hz by its own timer, which only runs while a
/// column is actually travelling.
final class RollingNumberView: NSView {
    var fontSize: CGFloat = 24 {
        didSet { if fontSize != oldValue { reloadMetrics() } }
    }
    var textColor: NSColor = .white {
        didSet { if !textColor.isEqual(oldValue) { applyColor() } }
    }

    private var font: NSFont = .monospacedDigitSystemFont(ofSize: 24, weight: .bold)
    /// Height of one digit slot. Cached: deriving it means building an
    /// `NSAttributedString` and asking Core Text for a font instance.
    private var lineHeight: CGFloat = 40
    private var glyphScale: CGFloat = 2
    private var widths: [Character: CGFloat] = [:]
    private var glyphImages: [Character: CGImage] = [:]
    private var digitStrip: CGImage?

    private var cols: [CALayer] = []
    private var masks: [CALayer] = []
    /// Non-nil only for digit columns: the strip that is slid to roll.
    private var rollMasks: [CALayer?] = []
    private var chars: [Character] = []
    /// Fractional position of each digit column, in digit units: 0.0 shows "0",
    /// 9.0 shows "9". Fractional is what makes the roll continuous.
    private var pos: [CGFloat] = []
    private var want: [CGFloat] = []
    private var inFlight = false
    private var text: String = ""
    private var rollTimer: Timer?
    private var lastRollTick: CFTimeInterval = 0

    /// Time constant of the roll. The old step was 0.35 per 80 ms frame, which
    /// is `1 - exp(-0.08 / 0.186)` — kept so the motion feels the same, but the
    /// curve is now evaluated per frame at 60 Hz instead of per 80 ms step.
    private static let rollTau: CGFloat = 0.186
    private static let rollInterval: TimeInterval = 1.0 / 60.0
    /// Half a row of padding above and below the ten digits: the strip can then
    /// sit between two digits without sampling past the image.
    private static let stripRows: CGFloat = 11

    override var intrinsicContentSize: NSSize {
        NSSize(width: min(naturalWidth, maxWidth) + 2, height: lineHeight)
    }

    /// Ceiling on the number's width, in points.
    ///
    /// Every other view in the panel is bounded — labels truncate, the track is
    /// 260 pt, the agent rows have a fixed 158 pt value column — and this is
    /// what keeps the number that way too. It is not merely cosmetic: a wider
    /// *intrinsic* width makes AppKit grow the window to meet its content's
    /// minimum size, and it never takes that back. Measured with a 19-digit
    /// value: the panel went 292 → 531 pt, and stayed at 531 pt after the value
    /// dropped back to 820,000 (STATUS.md 56).
    ///
    /// A backstop, not the working rule: the caller keeps the *string* inside the
    /// row (exact when it fits, the magnitude ladder when it does not — see
    /// `hudNumberText`, STATUS.md 60). Nothing is compressed horizontally any
    /// more; squeezing the glyphs to keep every digit read as broken typography.
    var maxWidth: CGFloat = 260 {
        didSet {
            guard maxWidth != oldValue else { return }
            invalidateIntrinsicContentSize()
            needsLayout = true
            placeColumns()
        }
    }

    /// Sum of the column widths — what the text needs if nothing constrains it.
    private var naturalWidth: CGFloat {
        text.reduce(CGFloat(0)) { $0 + charWidth($1) }
    }

    /// Width this view would need for `s`, without changing anything on screen —
    /// the caller uses it to choose between the exact number and the ladder.
    func width(of s: String) -> CGFloat {
        s.reduce(CGFloat(0)) { $0 + charWidth($1) }
    }

    /// True for the ten glyphs a digit column can display. Deliberately not
    /// `Character.isNumber` — the strip only has slots 0-9, and answering the
    /// same question here, in `charWidth` and in `rebuild` keeps the index
    /// arithmetic in `setText` safe.
    private func isColumnDigit(_ ch: Character) -> Bool {
        guard let v = ch.wholeNumberValue else { return false }
        return (0...9).contains(v)
    }

    private func charWidth(_ ch: Character) -> CGFloat {
        if let w = widths[ch] { return w }
        // Measure the real glyph for anything that is not a digit column: the
        // compact format carries 万/亿/K/M, and a 万 squeezed into a comma's
        // column clips.
        let s = isColumnDigit(ch) ? "0" : String(ch)
        let attr = NSAttributedString(string: s, attributes: [.font: font])
        let w = ceil(attr.size().width) + 4
        widths[ch] = w
        return w
    }

    func setText(_ s: String) {
        if s.count != text.count {
            text = s
            rebuild()
            invalidateIntrinsicContentSize()
            needsLayout = true
            return
        }
        guard s != text else { return }
        text = s
        for (i, ch) in s.enumerated() where i < chars.count {
            if chars[i] != ch {
                chars[i] = ch
                if !isColumnDigit(ch) {
                    masks[i].contents = glyphImage(ch)
                    placeColumns()      // a separator can change width
                }
            }
            guard isColumnDigit(ch) else { continue }
            let d = CGFloat(ch.wholeNumberValue ?? 0)
            if want[i] != d { want[i] = d; inFlight = true }
        }
        if inFlight { ensureRollTimer() }
    }

    private func reloadMetrics() {
        font = .monospacedDigitSystemFont(ofSize: fontSize, weight: .bold)
        let probe = NSAttributedString(string: "0", attributes: [.font: font])
        lineHeight = ceil(probe.size().height) + 10
        glyphScale = window?.backingScaleFactor ?? 2
        widths.removeAll()
        glyphImages.removeAll()
        digitStrip = nil
        rebuild()
        invalidateIntrinsicContentSize()
        needsLayout = true
    }

    override func layout() {
        super.layout()
        placeColumns()
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        // moving between displays with different scales: re-rasterise
        if (window?.backingScaleFactor ?? 2) != glyphScale { reloadMetrics() }
    }

    private func rebuild() {
        wantsLayer = true
        layer?.sublayers?.forEach { $0.removeFromSuperlayer() }
        cols = []; masks = []; rollMasks = []; chars = []; pos = []; want = []
        inFlight = false
        stopRollTimer()
        digitStrip = digitStrip ?? makeDigitStrip()
        for ch in text {
            let col = CALayer()
            col.backgroundColor = textColor.cgColor
            col.masksToBounds = true
            let mask = CALayer()
            mask.contentsScale = glyphScale
            mask.contentsGravity = .resize
            if isColumnDigit(ch) {
                let d = CGFloat(ch.wholeNumberValue ?? 0)
                mask.contents = digitStrip
                rollMasks.append(mask)
                // Snap rather than roll: a rebuild is a layout change (the
                // string grew or shrank), not a new value to count towards.
                pos.append(d)
                want.append(d)
            } else {
                mask.contents = glyphImage(ch)
                rollMasks.append(nil)
                pos.append(0)
                want.append(0)
            }
            col.mask = mask
            layer?.addSublayer(col)
            cols.append(col)
            masks.append(mask)
            chars.append(ch)
        }
        placeColumns()
    }

    /// Put every column — and every column's mask — where the current widths and
    /// roll positions say. Cheap, and the only place geometry is written.
    private func placeColumns() {
        noAnimation {
            var x: CGFloat = 0
            for i in 0..<cols.count {
                let w = charWidth(chars[i])
                cols[i].frame = CGRect(x: x, y: 0, width: w, height: lineHeight)
                if rollMasks[i] != nil {
                    masks[i].frame = CGRect(x: 0, y: stripOriginY(pos[i]),
                                            width: w, height: Self.stripRows * lineHeight)
                } else {
                    masks[i].frame = CGRect(x: 0, y: 0, width: w, height: lineHeight)
                }
                x += w
            }
        }
    }

    /// Move every column one frame's worth towards its target and repaint only
    /// what moved. Exponential approach is monotonic and survives any number of
    /// interruptions because there is nothing to interrupt; the snap threshold
    /// is what guarantees arrival.
    private func advance(_ dt: CGFloat) {
        guard inFlight else { return }
        let k = 1 - exp(-dt / Self.rollTau)
        var moving = false
        noAnimation {
            for i in 0..<cols.count where rollMasks[i] != nil {
                let d = want[i] - pos[i]
                if abs(d) < 0.02 {
                    if pos[i] != want[i] {
                        pos[i] = want[i]
                        offsetMask(i)
                    }
                } else {
                    pos[i] += d * k
                    moving = true
                    offsetMask(i)
                }
            }
        }
        inFlight = moving
        if !moving { stopRollTimer() }
    }

    private func offsetMask(_ i: Int) {
        masks[i].frame.origin = CGPoint(x: 0, y: stripOriginY(pos[i]))
    }

    /// Where the digit strip has to sit for a column holding fractional value
    /// `p`. Digit `d` is drawn in the row band `d + 0.5 ... d + 1.5` measured
    /// from the *bottom* of the strip (half a row of padding at each end), so
    /// its slot is centred `(d + 1)` rows up. Sliding that centre onto the
    /// centre of a one-row column puts the strip's origin at `-(p + 0.5)`.
    private func stripOriginY(_ p: CGFloat) -> CGFloat {
        -(p + 0.5) * lineHeight
    }

    private func ensureRollTimer() {
        guard rollTimer == nil else { return }
        lastRollTick = CACurrentMediaTime()
        let t = Timer(timeInterval: Self.rollInterval, repeats: true) { [weak self] _ in
            guard let self else { return }
            let now = CACurrentMediaTime()
            let dt = CGFloat(min(max(now - self.lastRollTick, 0), 0.1))
            self.lastRollTick = now
            self.advance(dt)
        }
        // .common, so the digits keep rolling while a window drag or menu
        // tracking is running the run loop in another mode
        RunLoop.main.add(t, forMode: .common)
        rollTimer = t
    }

    private func stopRollTimer() {
        rollTimer?.invalidate()
        rollTimer = nil
    }

    private func applyColor() {
        for c in cols { c.backgroundColor = textColor.cgColor }
    }

    /// Layer geometry must not pick up Core Animation's implicit 0.25 s
    /// animation — the roll is already animated by `advance`.
    private func noAnimation(_ body: () -> Void) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        body()
        CATransaction.commit()
    }

    // MARK: - Glyph rasterisation (white on clear — used as layer masks)

    /// The ten digits as one vertical strip: slot `d` holds digit `d`, with half
    /// a slot of padding above and below. Rows are counted from the bottom,
    /// which is the space `stripOriginY` is derived in.
    private func makeDigitStrip() -> CGImage? {
        let w = charWidth("0")
        let h = lineHeight
        return rasterise(CGSize(width: w, height: Self.stripRows * h)) { [self] _ in
            for d in 0...9 {
                drawGlyph(String(d), in: CGRect(x: 0, y: (CGFloat(d) + 0.5) * h, width: w, height: h))
            }
        }
    }

    private func glyphImage(_ ch: Character) -> CGImage? {
        if let g = glyphImages[ch] { return g }
        let w = charWidth(ch)
        let img = rasterise(CGSize(width: w, height: lineHeight)) { [self] _ in
            drawGlyph(String(ch), in: CGRect(x: 0, y: 0, width: w, height: lineHeight))
        }
        glyphImages[ch] = img
        return img
    }

    private func drawGlyph(_ s: String, in rect: CGRect) {
        let attr = NSAttributedString(string: s, attributes: [.font: font,
                                                              .foregroundColor: NSColor.white])
        let size = attr.size()
        attr.draw(at: CGPoint(x: rect.midX - size.width / 2, y: rect.midY - size.height / 2))
    }

    /// Rasterise at the backing scale.
    ///
    /// `draw` runs in the ordinary y-up space, and the image that comes out
    /// carries that space unchanged: high y in here is the top row of the
    /// bitmap, which is where Core Animation puts it when the bitmap is used as
    /// layer contents. (Flipping the context instead — the obvious way to get a
    /// top-left origin — mirrors every glyph and puts drawing y=0 at the
    /// bitmap's *bottom*, which is what the first cut of this did.)
    private func rasterise(_ size: CGSize, _ draw: (CGRect) -> Void) -> CGImage? {
        let scale = glyphScale
        let pxW = max(1, Int((size.width * scale).rounded()))
        let pxH = max(1, Int((size.height * scale).rounded()))
        guard let ctx = CGContext(data: nil, width: pxW, height: pxH, bitsPerComponent: 8,
                                  bytesPerRow: 0, space: CGColorSpaceCreateDeviceRGB(),
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.clear(CGRect(x: 0, y: 0, width: pxW, height: pxH))
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: false)
        ctx.scaleBy(x: scale, y: scale)
        draw(CGRect(origin: .zero, size: size))
        NSGraphicsContext.restoreGraphicsState()
        return ctx.makeImage()
    }
}

/// Edge-docked bubble: today's token total (abbreviated) inside a glass disc
/// whose ring spins at a speed proportional to the live burn rate. The ring
/// keeps the HUD's heat gradient (idle near-white → burning red), so the
/// odometer's colour language survives in bubble form.
final class RadialHUDView: NSView {
    var text: String = "0" {
        didSet { if oldValue != text { needsDisplay = true } }
    }
    /// Heat colour — pushed from animStep so both UIs share one palette.
    var color: NSColor = .white {
        didSet { needsDisplay = true }
    }
    /// tokens/min — maps to ring revolutions per second.
    var burn: Double = 0
    /// Seconds since the newest record from *any* agent. Pushed from the live
    /// poll and advanced locally between polls; this, not `burn`, is what says
    /// whether anything is still working.
    var idleFor: Double = 999
    /// Mirrors the HUD theme so the disc is dark glass on dark glass and a
    /// frosted white pane on light glass (same language as the panel).
    var isDark: Bool = true {
        didSet { if oldValue != isDark { needsDisplay = true } }
    }

    var onClick: (() -> Void)?
    var menuHandler: (() -> NSMenu)?

    private var angle: CGFloat = -.pi / 2          // start at 12 o'clock
    private var lastTick = CACurrentMediaTime()
    private var tracking: NSTrackingArea?
    /// Angular speed actually being used, in revolutions/sec. Chases the
    /// speed implied by `burn` rather than snapping to it.
    private var speed: Double = 0
    /// Trail length actually being drawn, in fractions of a revolution.
    private var sweep: CGFloat = 0.30

    /// Resting trail length, in fractions of a revolution.
    private static let restSweep: CGFloat = 0.30

    /// The speed the ring is heading for right now.
    ///
    /// `burn` alone is the wrong stopping signal: it is the server's 60-second
    /// window, so it holds its full value until records start ageing out and
    /// only reaches zero a whole minute after the last token. Measured over 378
    /// real work sessions, the previous speed — a hardcoded 0.16 rev/s idle
    /// creep on top of it — never once fell below 0.05 rev/s. It never stopped.
    ///
    /// `idleFor` is the age of the newest record, which does say whether
    /// anything is still working, so the burn magnitude is multiplied by a
    /// freshness ramp derived from it. The two constants come from the measured
    /// distribution of gaps *within* a work session (n = 25 022): median 6.5 s,
    /// p75 14.2 s, p90 30.1 s, with 29 % of gaps reaching 12 s. Ten seconds of
    /// full-speed grace therefore clears the median gap with room to spare,
    /// while completing at 32 s sits past p90 — a normal think pause throttles
    /// the ring without stalling it, and a genuine stop winds down in ~34 s.
    /// Replaying 312 substantial sessions confirmed the ramp adds no restarting
    /// of its own (685 vs 613 ungated, across 312 sessions): it does not flicker.
    private var targetSpeed: Double {
        let fresh = max(0, min(1, 1 - (idleFor - 10) / 22))
        // a hot burn spins several rev/s; the cap keeps it readable
        return min(burn / 2000.0, 2.6) * 1.5 * fresh
    }

    /// True once the ring has wound all the way down to a standstill *and*
    /// nothing is asking it to turn. The owner uses this to stop the 30 fps
    /// timer: the bubble is idle most of the time, and there is nothing to gain
    /// from repainting a still image at 30 fps.
    ///
    /// The `targetSpeed` clause is load-bearing. Without it the condition stays
    /// true forever once the timer stops — `advance()` is what sets `speed`, and
    /// it is not being called — so the ring could never wake up again.
    ///
    /// The sweep tolerance is loose on purpose. The trail eases towards its
    /// resting length asymptotically, and requiring it within 0.72° of 108°
    /// made this only just satisfiable (measured: 108.7°); anything tighter and
    /// the ring would never be considered at rest at all, leaving it repainting
    /// at 30 fps forever. 1.8° is not visible on a 76 pt bubble.
    var isAtRest: Bool {
        speed == 0 && abs(sweep - Self.restSweep) < 0.005 && targetSpeed == 0
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let t = tracking { removeTrackingArea(t) }
        let t = NSTrackingArea(rect: bounds,
                               options: [.mouseEnteredAndExited, .activeAlways],
                               owner: self, userInfo: nil)
        addTrackingArea(t)
        tracking = t
    }
    override func mouseEntered(with e: NSEvent) { NSCursor.pointingHand.push() }
    override func mouseExited(with e: NSEvent) { NSCursor.pop() }

    /// Advance the ring — call at ~30fps while docked.
    ///
    /// The ring shows **burn**, not "busy": the target comes from `targetSpeed`
    /// (see there for the measured constants). What is left here is the motion —
    /// the lag is what makes the ring feel like it has mass rather than acting
    /// like a slider following a value. It spins up in 0.55 s but coasts down
    /// over 1.6 s, and the trail length follows the *smoothed* speed on a slower
    /// constant still, so a burst pours the tail out behind the head and it
    /// reels back in afterwards. Because the tail sits at `head - sweep`,
    /// growing the sweep slides it backwards along the ring — the trail is laid
    /// down, not merely scaled.
    func advance() {
        let now = CACurrentMediaTime()
        let dt = min(max(now - lastTick, 0), 0.12)
        lastTick = now

        let target = targetSpeed
        let tau = target > speed ? 0.55 : 1.6
        speed += (target - speed) * (1 - exp(-dt / tau))
        // exponential decay only approaches zero, and a ring creeping at
        // 0.01 rev/s is a ring that never stops — snap it, so `isAtRest` can
        // actually become true
        if target <= 0 && speed < 0.01 { speed = 0 }

        let wasSettling = abs(sweep - Self.restSweep) > 0.0005
        angle += CGFloat(dt * speed) * 2 * .pi
        if angle > 2 * .pi { angle -= 2 * .pi }

        // ~108° at rest, up to ~245° under sustained load. The time constant is
        // deliberately a touch under a second: measured against a real burn
        // profile, a 1.8 s constant meant the trail was still paying out when a
        // typical one-second burst ended, so the extension was never seen. What
        // it must not be is instantaneous — the trail has to be *laid down*.
        let sweepTarget = Self.restSweep + CGFloat(min(speed / 3.2, 1.0)) * 0.38
        sweep += (sweepTarget - sweep) * CGFloat(1 - exp(-dt / 1.1))

        // only repaint while something is actually changing; a still ring is the
        // common case and should cost nothing
        if speed > 0 || wasSettling { needsDisplay = true }
    }

    func resetClock() {
        lastTick = CACurrentMediaTime()
        // a fresh dock starts from rest and spins up if there is work to show,
        // rather than inheriting the previous session's momentum
        speed = 0
        sweep = Self.restSweep
    }

    override func mouseUp(with e: NSEvent) { onClick?() }
    override func menu(for event: NSEvent) -> NSMenu? { menuHandler?() ?? super.menu(for: event) }

    override func draw(_ dirty: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        var style = RingRenderer.Style.forTheme(dark: isDark)
        style.sweep = sweep * 2 * .pi

        let center = NSPoint(x: bounds.midX, y: bounds.midY)
        // The comet's bloom bleeds well past the band, and the panel is a fixed
        // 76pt, so the glass is inset a little to keep the disc's own drop
        // shadow inside. The last point or two of the blur gets clipped at the
        // panel edge, where its alpha is under the noise floor anyway.
        let glassR = min(bounds.width, bounds.height) / 2 - 2
        let radius = glassR - style.headWidth / 2 - 1.5
        let discRect = NSRect(x: center.x - glassR, y: center.y - glassR,
                              width: glassR * 2, height: glassR * 2)

        // glass disc with a soft drop shadow — dark glass on dark, a frosted
        // white pane on light (same material language as the panel itself)
        ctx.saveGState()
        ctx.setShadow(offset: .zero, blur: 9,
                      color: NSColor.black.withAlphaComponent(isDark ? 0.38 : 0.18).cgColor)
        (isDark ? NSColor.black.withAlphaComponent(0.34)
                : NSColor.white.withAlphaComponent(0.72)).setFill()
        NSBezierPath(ovalIn: discRect).fill()
        ctx.restoreGState()

        // comet ring: tapered ribbon, conic opacity trail, real bloom
        RingRenderer.draw(in: ctx, center: center, radius: radius,
                          headAngle: angle, color: color, style: style,
                          dark: isDark, drawTrack: true)

        // centre label — abbreviated token total, shrunk to fit the disc
        let inner = radius * 2 * 0.72
        var size: CGFloat = 12.5
        var s = NSAttributedString(string: text, attributes: [
            .font: NSFont.monospacedDigitSystemFont(ofSize: size, weight: .bold),
            .foregroundColor: color,
        ])
        while s.size().width > inner && size > 8 {
            size -= 0.5
            s = NSAttributedString(string: text, attributes: [
                .font: NSFont.monospacedDigitSystemFont(ofSize: size, weight: .bold),
                .foregroundColor: color,
            ])
        }
        let sz = s.size()
        s.draw(at: NSPoint(x: center.x - sz.width / 2, y: center.y - sz.height / 2))
    }
}
