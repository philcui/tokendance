import AppKit
import WebKit
import CryptoKit

// Token Usage Center — native macOS menu bar app
// RunCat-style animated cat: run speed = token burn rate
// Data: http://127.0.0.1:8737  (/api/live polled at 1s)

final class AppDelegate: NSObject, NSApplicationDelegate, WKUIDelegate {
    var statusItem: NSStatusItem!
    var dashboardWindow: NSWindow?
    var dashboardWeb: WKWebView?
    var hudPanel: NSPanel?
    var hudStack: NSStackView?
    var hudTitleRow: NSView?
    var hudBurn: RollingNumberView?
    var hudUnit: NSTextField?
    var hudEst: NSTextField?
    var hudStatus: NSTextField?
    var hudTodayCap: NSTextField?
    var hudBarFill: NSView?
    var hudBarTrack: NSView?
    var hudRate: NSTextField?
    var hudRank: NSTextField?
    // HUD shows only today's top-3 agents — rows are re-bound every poll
    var hudRows: [(dot: NSView, name: NSTextField, val: NSTextField, row: NSView)] = []
    var hudCompact = false
    var hudResizing = false
    var hudCollapsible: [NSView] = []
    var hudCollapseBtn: NSButton?
    var hudExpandBtn: NSButton?
    /// 挂件上的"有新版本"标记：正常模式在标题行，紧凑模式在数字行（两个实例，
    /// 一个视图只能有一个父视图）。都只在检测到新版本时出现。
    var hudUpdateBadge: NSButton?
    var hudUpdateBadgeCompact: NSButton?
    var hudEffect: NSVisualEffectView?
    var hudSide: RadialHUDView?
    var hudDocked = false
    var hudRestoreFrame: NSRect = .zero
    /// True once the user has dragged the panel themselves, or once a position
    /// they chose earlier has been restored. From then on the top-right corner
    /// is only a *default*: the app never yanks the panel back to it.
    ///
    /// This is the bug that made the HUD feel nailed down. `positionHUD()` is
    /// called from `applyLive()` on every 1 s poll and used to force the corner
    /// unconditionally, so a drag survived at most a second before being undone
    /// — the panel appeared immovable. It matters beyond convenience: a HUD
    /// parked over the top-right corner of the screen sits on top of other
    /// apps' toolbars and window controls.
    var hudUserMoved = false
    /// True between mouse-down on the panel and the end of that gesture. The
    /// window is being moved by the system for the whole of it, so nothing else
    /// may touch the panel's frame — see `positionHUD()`.
    var hudDragging = false
    var spinTimer: Timer?
    var records: [[String: Any]] = []
    let api = URL(string: "http://127.0.0.1:8737/api/data")!
    let liveApi = URL(string: "http://127.0.0.1:8737/api/live")!
    /// Source names in agent-id order. The server owns this list — it ships with
    /// `/api/data` and is what `aggregate()` sizes its per-agent arrays from, so
    /// adding a source to the server is enough for it to appear here. Starting
    /// empty is deliberate: a local copy could disagree with the parsers, and
    /// the only cost is that the menu and the HUD's agent rows stay empty for
    /// the first second of a launch.
    var agentNames: [String] = []
    let agentColors: [NSColor] = [NSColor(red: 0.29, green: 0.56, blue: 1.0, alpha: 1),
                                  NSColor(red: 1.0, green: 0.45, blue: 0.16, alpha: 1),
                                  NSColor(red: 0.22, green: 0.78, blue: 0.35, alpha: 1),
                                  NSColor(red: 0.55, green: 0.40, blue: 1.0, alpha: 1),
                                  NSColor(red: 0.90, green: 0.30, blue: 0.55, alpha: 1),
                                  NSColor(red: 0.20, green: 0.70, blue: 0.75, alpha: 1),
                                  NSColor(red: 0.85, green: 0.47, blue: 0.02, alpha: 1),
                                  NSColor(red: 0.88, green: 0.11, blue: 0.28, alpha: 1),
                                  NSColor(red: 0.31, green: 0.27, blue: 0.90, alpha: 1),
                                  NSColor(red: 0.40, green: 0.64, blue: 0.03, alpha: 1)]
    var hudVisible = true
    /// KVO handle: system light/dark flips must repaint the glass immediately
    var appearanceObs: NSKeyValueObservation?
    /// Last theme actually pushed into AppKit — makes applyThemeToHUD() free
    /// to call on every poll (it becomes a no-op when nothing changed).
    var appliedDark: Bool?
    var appliedTheme: String?

    // cat animation state — run uses 12 phases; bike uses 24 phases so that at
    // the same (macOS-capped) frame rate each step is half the angular distance:
    // visibly smoother spinning without touching swap frequency.
    enum Gait { case run, bike }
    static let runFrames: [NSImage] = (0..<12).map { gaitFrame(.run, $0) }
    static let bikeFrames: [NSImage] = (0..<24).map { gaitFrame(.bike, $0) }
    var frameIdx = 0
    var animAccum = 0.0
    var frameInterval = 0.0  // seconds per frame; 0 = paused (idle)
    var useBike = false      // hysteresis: bikes above ~55% heat, runs below 40%



    // MARK: - Procedural menu bar frames (template images adapt to menu bar)

    private static func line(_ p: NSBezierPath, _ x1: Double, _ y1: Double, _ x2: Double, _ y2: Double) {
        p.move(to: NSPoint(x: x1, y: y1))
        p.line(to: NSPoint(x: x2, y: y2))
    }

    static func gaitFrame(_ gait: Gait, _ phase: Int) -> NSImage {
        let steps = gait == .bike ? 24 : 12
        let t = Double(phase) / Double(steps) * 2 * Double.pi
        let img = NSImage(size: NSSize(width: 30, height: 16))
        img.lockFocus()
        NSColor.black.setFill(); NSColor.black.setStroke()
        switch gait {
        case .run: drawRunner(t)
        case .bike: drawBiker(t)
        }
        img.unlockFocus()
        img.isTemplate = true
        return img
    }

    // running cat — gallop gait, smooth sine legs across 12 phases
    private static func drawRunner(_ t: Double) {
        let o: Double = 4
        let c = NSColor.black
        c.setFill(); c.setStroke()
        NSBezierPath(roundedRect: NSRect(x: o+3, y: 5.5, width: 12, height: 5.5),
                     xRadius: 2.7, yRadius: 2.7).fill()
        NSBezierPath(ovalIn: NSRect(x: o+14, y: 7.5, width: 5.5, height: 5.5)).fill()
        for ex in [o+15.2, o+17.2] {
            let ear = NSBezierPath()
            ear.move(to: NSPoint(x: ex, y: 12.4))
            ear.line(to: NSPoint(x: ex + 0.8, y: 14.6))
            ear.line(to: NSPoint(x: ex + 1.6, y: 12.4))
            ear.close()
            ear.fill()
        }
        let tail = NSBezierPath()
        tail.move(to: NSPoint(x: o+3.2, y: 9.5))
        tail.curve(to: NSPoint(x: o+0.8, y: 14.2),
                   controlPoint1: NSPoint(x: o+1.2, y: 9.5),
                   controlPoint2: NSPoint(x: o+0.6, y: 12))
        tail.lineWidth = 1.3
        tail.lineCapStyle = .round
        tail.stroke()
        let legsX = [o+5.2, o+7.6, o+10.8, o+13.2]
        let legs = NSBezierPath()
        legs.lineWidth = 1.3
        legs.lineCapStyle = .round
        for (i, x0) in legsX.enumerated() {
            let dx = sin(t + (i < 2 ? .pi : 0)) * 1.6
            legs.move(to: NSPoint(x: x0, y: 6.2))
            legs.line(to: NSPoint(x: x0 + dx, y: 1.4))
        }
        legs.stroke()
    }

    // cat riding a bike — redesigned for smoothness:
    //   wheels: tire ring + 3 spokes + valve dot, continuous rotation
    //   crank: pedals on a rotating circle, legs follow via 2-bone IK
    //     (hip -> knee -> pedal, knee from circle-circle intersection)
    //   body: subtle bob at crank frequency (torso, head, ears ride together)
    private static func drawBiker(_ t: Double) {
        let c = NSColor.black
        c.setStroke()
        let rearX = 6.0, frontX = 23.0, rearY = 4.4, frontY = 4.4
        let bbX = 14.5, bbY = 6.0            // bottom bracket (crank center)
        let crankR = 2.1
        let body = NSBezierPath()
        body.lineWidth = 1.4
        body.lineCapStyle = .round
        for (cx, cy) in [(rearX, rearY), (frontX, frontY)] {  // wheels
            body.append(NSBezierPath(ovalIn: NSRect(x: cx-3.4, y: cy-3.4, width: 6.8, height: 6.8)))
            for k in 0..<3 {
                let a = t + Double(k) * 2.094
                line(body, cx, cy, cx + 2.7*cos(a), cy + 2.7*sin(a))
            }
            // valve dot just outside the spokes for readable rotation
            let va = t * 1.0 + 0.7
            NSBezierPath(ovalIn: NSRect(x: cx + 2.2*cos(va) - 0.4, y: cy + 2.2*sin(va) - 0.4,
                                        width: 0.8, height: 0.8)).fill()
        }
        line(body, rearX, rearY, bbX, bbY)   // chainstay
        line(body, bbX, bbY, frontX, frontY) // down tube
        line(body, 11, 10.2, bbX, bbY)       // seat tube
        line(body, bbX, bbY, 20, 10.6)       // top tube
        line(body, 20, 10.6, frontX, frontY) // fork
        line(body, 9.8, 10.6, 12.2, 10.6)    // saddle
        line(body, 19, 11.4, 21.4, 10.9)     // handlebar
        body.stroke()
        let bob = sin(t * 2.0) * 0.22        // one bob per pedal stroke
        let hip = NSPoint(x: 11, y: 10.2 + bob)
        let shoulder = NSPoint(x: 17.6, y: 13.2 + bob)
        let rider = NSBezierPath()           // torso
        rider.lineWidth = 1.6
        rider.lineCapStyle = .round
        rider.move(to: hip)
        rider.line(to: shoulder)
        rider.stroke()
        NSBezierPath(ovalIn: NSRect(x: 17.2, y: 13.4 + bob, width: 3.4, height: 3.2)).fill()  // head
        for ex in [17.4, 18.6] {             // ears
            let ear = NSBezierPath()
            ear.move(to: NSPoint(x: ex, y: 16.4 + bob))
            ear.line(to: NSPoint(x: ex+0.5, y: 17.6 + bob))
            ear.line(to: NSPoint(x: ex+1.0, y: 16.4 + bob))
            ear.close()
            ear.fill()
        }
        let arm = NSBezierPath()
        arm.lineWidth = 1.2
        arm.lineCapStyle = .round
        arm.move(to: NSPoint(x: shoulder.x - 0.2, y: shoulder.y - 0.2))
        arm.line(to: NSPoint(x: 20.2, y: 11.0))
        arm.stroke()
        for dt in [0.0, Double.pi] {         // pedaling legs, 2-bone IK
            let px = bbX + crankR*cos(t+dt), py = bbY + crankR*sin(t+dt)
            let knee = ikKnee(hip, NSPoint(x: px, y: py), thigh: 3.4, shin: 3.6, forward: true)
            let leg = NSBezierPath()
            leg.lineWidth = 1.3
            leg.lineCapStyle = .round
            leg.move(to: hip)
            leg.line(to: knee)
            leg.line(to: NSPoint(x: px, y: py))
            leg.stroke()
        }
        // crank arm (rear pedal only, the front one is hidden by the leg)
        let crank = NSBezierPath()
        crank.lineWidth = 1.1
        crank.lineCapStyle = .round
        line(crank, bbX, bbY, bbX + crankR*cos(t), bbY + crankR*sin(t))
        crank.stroke()
    }

    // two-bone IK: knee position where thigh and shin meet the pedal,
    // bent forward (toward the front wheel)
    private static func ikKnee(_ hip: NSPoint, _ foot: NSPoint, thigh: Double, shin: Double, forward: Bool) -> NSPoint {
        let dx = foot.x - hip.x, dy = foot.y - hip.y
        var d = sqrt(dx*dx + dy*dy)
        let maxReach = thigh + shin - 0.05
        if d > maxReach { d = maxReach }
        if d < 0.3 { d = 0.3 }
        let a = (thigh*thigh - shin*shin + d*d) / (2*d)
        let h = sqrt(max(thigh*thigh - a*a, 0))
        let mx = hip.x + a*dx/d, my = hip.y + a*dy/d
        let sgn: Double = forward ? -1 : 1   // perpendicular side; y-up flips it
        return NSPoint(x: mx + sgn*h*(-dy)/d, y: my + sgn*h*dx/d)
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        installMainMenu()
        statusItem = NSStatusBar.system.statusItem(withLength: 34)  // icon-only:
        // the menu bar on this machine is packed; anything wider gets evicted
        // to the overflow slot (screen far-left, hidden under app menus)
        if let btn = statusItem.button {
            btn.image = menuBarImage(Self.runFrames[0])
            btn.imagePosition = .imageOnly
        }
        statusItem.menu = buildMenu()
        refresh()
        Timer.scheduledTimer(withTimeInterval: 60.0, repeats: true) { [weak self] _ in
            self?.refresh()
        }
        pollLive()
        // Every HUD timer goes into .common mode, not just the default mode:
        // while the panel is being dragged — or a menu is tracking — the run
        // loop is in another mode, and a default-mode timer simply stops. The
        // number and the ring must keep moving for the whole gesture.
        addTimer(1.0) { [weak self] in self?.pollLive() }
        addTimer(0.08) { [weak self] in self?.animStep() }   // RunCat/animation loop
        showHUD()
        // system light/dark flip → repaint the glass immediately. The 1s live
        // poll runs through the same guard as a fallback, so even if KVO ever
        // misses a delivery the HUD is at most one second behind.
        appearanceObs = NSApp.observe(\.effectiveAppearance, options: [.new]) { [weak self] _, _ in
            DispatchQueue.main.async { self?.applyThemeToHUD() }
        }
        // NSScreen may be unpopulated this early — retry placement, and
        // re-place whenever the display configuration changes
        for delay in [0.5, 1.5, 3.0] {
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
                self?.positionHUD()
                self?.hudPanel?.orderFrontRegardless()
            }
        }
        NotificationCenter.default.addObserver(forName: NSApplication.didChangeScreenParametersNotification,
                                               object: nil, queue: .main) { [weak self] _ in
            self?.positionHUD()
        }
        // 匿名统计：默认开启（用户明确关过的会一直保持关闭），之后每 6 小时一条。
        // 延后 3 秒是避免和 HUD 首帧抢主线程，与询问无关（弹窗已经取消了）。
        DispatchQueue.main.asyncAfter(deadline: .now() + 3) { [weak self] in
            self?.pingTelemetry()
        }
        Timer.scheduledTimer(withTimeInterval: 6 * 3600.0, repeats: true) { [weak self] _ in
            self?.pingTelemetry()
        }
        checkUpdate()
        Timer.scheduledTimer(withTimeInterval: 24 * 3600.0, repeats: true) { [weak self] _ in
            self?.checkUpdate()
        }
    }

    // animation + extrapolation state
    var dispBurn: Double = 0
    var lastTi: Double = 0
    var lastB60: Double = 0
    var lastAgo: Double = 999
    /// Whether the server has ever reported an activity timestamp. `last_ago_s`
    /// is null (not 0) when there is no record at all — a brand-new machine —
    /// and the 999 below is only a local placeholder for the arithmetic. Before
    /// this flag the two were indistinguishable and a fresh install announced
    /// "cooling · last active 999s ago", i.e. a magic number on screen.
    var sawActivity = false
    var lastLiveAt: TimeInterval = 0
    /// Last value actually pushed at the HUD, so a frame that changes nothing
    /// costs nothing. Every one of these assignments used to run 12.5 times a
    /// second whether or not the value had moved: the colour writes re-walked
    /// every digit label the odometer owned, and the bar's width is an Auto
    /// Layout constraint whose constant change invalidates the whole panel's
    /// layout. During a drag that layout pass competed with the drag itself.
    private var shownBurnText = ""
    private var shownHeatBucket = Int.min
    private var shownBurst = false
    // The stretchy bar is a layer transform; this remembers what it currently is
    // so an unchanged value does not touch the layer at all.
    private var shownBarRatio: CGFloat = -1
    private var smoothTimer: Timer?
    private var lastSmoothAt: CFTimeInterval = 0
    /// Heat is quantised for the colour ramp: 1/512 of the ramp is far below
    /// what a human can see on a 24 pt digit, and it turns a continuous stream
    /// of colour writes into a handful.
    private static let heatBuckets = 512.0

    func animStep() {
        // continuous extrapolation every frame; landings correct the value and
        // the odometer rolls through the difference (forward OR backward) with
        // distance-scaled duration — spinning, never teleporting, never frozen
        let now = CACurrentMediaTime()
        dispBurn = lastB60 > 0 && lastAgo < 20
            ? lastTi + lastB60 / 60.0 * min(lastAgo + (now - lastLiveAt), 20)
            : lastTi
        // Anything but "precise" shows a magnitude — 612万 / 6.1M / 612百万 — so
        // the odometer's display granularity coarsens and its digits change far
        // less often; "precise" keeps every digit and relies on the frame-driven
        // roll alone. "compact" is the automatic magnitude (the largest unit
        // that fits); "k" / "m" / "b" pin one of the three familiar levels. The
        // unit label carries the ≈ estimate only in precise mode; in the
        // magnitude modes it would repeat the number itself.
        let style = hudNumberStyle(safeInt(dispBurn.rounded()))
        if style.text != shownBurnText {
            shownBurnText = style.text
            hudBurn?.setText(style.text)
        }
        if let u = hudUnit {
            if u.stringValue != style.label { u.stringValue = style.label }
            if u.isHidden != style.labelHidden { u.isHidden = style.labelHidden }
        }
        // Eased heat drives the colour transitions and the stretchy bar. The
        // easing itself now happens in `smoothStep` (60 Hz, only while the value
        // is actually moving); this timer just makes sure it is running.
        if abs(lastHeat - dispHeat) >= 0.002 { startSmooth() } else { stopSmooth() }
        let c = burnColor(dispHeat)
        let bucket = Int((dispHeat * Self.heatBuckets).rounded())
        if bucket != shownHeatBucket {
            shownHeatBucket = bucket
            hudBurn?.textColor = c
            hudBarFill?.layer?.backgroundColor = (dispHeat > 0.02
                ? c
                : (isDarkUI ? NSColor(white: 0.45, alpha: 1)
                            : NSColor(white: 0.78, alpha: 1))).cgColor
        }
        if lastBurst != shownBurst {
            shownBurst = lastBurst
            hudStatus?.textColor = lastBurst ? c : .secondaryLabelColor
        }
        // docked bubble shares the same eased heat colour; the burn rate sets
        // how fast the ring turns, and how long ago the last record landed sets
        // whether it turns at all
        if hudDocked, let side = hudSide {
            side.color = c
            side.burn = lastB60
            // the server's figure is the age at the moment it answered; carry it
            // forward locally so the ring keeps winding down between polls
            side.idleFor = lastAgo + (now - lastLiveAt)
            // the bubble is a 76 pt disc: it keeps the automatic magnitude even
            // when the big number is pinned to a level that does not fit there
            side.text = abbrevAuto(safeInt(dispBurn.rounded()))
            // once the ring is genuinely still, stop the 30 fps timer — an idle
            // bubble repainting 30 times a second is pure waste. animStep runs
            // on its own 0.08 s timer, so it is what wakes the ring back up.
            if side.isAtRest {
                stopSpin()
            } else if spinTimer == nil {
                startSpin()
            }
        }
        let frames = useBike ? Self.bikeFrames : Self.runFrames
        guard frameInterval > 0 else {
            if frameIdx != 0 { frameIdx = 0; statusItem.button?.image = menuBarImage(frames[0]) }
            return
        }
        animAccum += 0.08
        if animAccum >= frameInterval {
            animAccum = 0
            frameIdx = (frameIdx + 1) % frames.count
            let img = frames[frameIdx]
            if statusItem.button?.image !== img { statusItem.button?.image = img }
        }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        showHUD()
        // Dock click. Bringing the panel back is the whole point; the full
        // dashboard only opens if nothing of ours was on screen at all (the
        // HUD panel counts as a visible window, so this stays quiet while the
        // panel is up).
        if !flag { openDashboard() }
        return true
    }

    /// The bundled server is this app's child process, so it has to go when the
    /// app goes. Nothing was stopping it before: quit the app and the server
    /// kept holding port 8737 until logout. That was survivable while quitting
    /// was rare (no Dock icon, so the only ways out were the menu-bar item and
    /// Activity Monitor), but ⌘Q is now one keystroke away.
    ///
    /// Only our own child is touched — if the port was already being served by
    /// something the user started, `serverProc` is nil and we leave it alone.
    func applicationWillTerminate(_ notification: Notification) {
        guard let p = serverProc, p.isRunning else { return }
        p.terminate()
        // The server exits promptly on SIGTERM; give it a moment so the port is
        // actually free before the next launch, without ever hanging the quit.
        let deadline = Date().addingTimeInterval(2.0)
        while p.isRunning && Date() < deadline { usleep(50_000) }
        if p.isRunning { kill(p.processIdentifier, SIGKILL) }
    }

    // MARK: - Full data refresh (totals, menus, dashboard aggregates)

    func refresh() {
        URLSession.shared.dataTask(with: api) { [weak self] data, _, _ in
            guard let self else { return }
            guard let data,
                  let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let recs = obj["records"] as? [[String: Any]] else {
                DispatchQueue.main.async {
                    self.statusItem.button?.title = self.L(" 启动中…", " Starting…")
                    // Retry quickly instead of waiting for the 60 s tick.
                    //
                    // The first fetch races the local server's own start-up: the
                    // app spawns that server when port 8737 is not answering, and
                    // this request can lose. It used to mean *no agent names for a
                    // whole minute* — and with `agentNames` empty the ranking rows
                    // at the bottom of the HUD were all hidden, which is exactly
                    // the "sometimes the rows are just not there" the user saw.
                    // Measured before the fix: FAILED at t=0, ok at t=+59 s.
                    if self.refreshRetry < 15 {          // ~30 s of trying
                        self.refreshRetry += 1
                        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { self.refresh() }
                    }
                }
                return
            }
            // Publish on the main thread. `records` is read there by the
            // aggregate pass (menu build + the 60 s refresh), and storing it
            // from this URLSession callback thread would race that read: ARC
            // would be releasing the array the main thread is walking, which is
            // an over-release, i.e. a crash. The expensive part — parsing 3 MB
            // of JSON into 25 k dictionaries — has already happened off-main,
            // and assigning here is a single retain.
            //
            // `aggGen` and the cache are bumped in the same hop so the aggregate
            // cache is only ever touched from one thread.
            DispatchQueue.main.async {
                self.records = recs
                // the source list rides along with the data, and has to be in
                // place *before* the aggregate below sizes its arrays from it
                if let names = obj["agents"] as? [String], names != self.agentNames {
                    self.agentNames = names
                    self.todayPerAgent = Array(repeating: Array(repeating: 0, count: 4),
                                               count: names.count)
                }
                self.aggGen += 1
                self.refreshRetry = 0
                self.updateMenuAndTitle()
            }
        }.resume()
    }

    // MARK: - Live burn metrics (every 1s)

    // MARK: - Backend supervision
    //
    // Every number this app shows comes from the local server on
    // 127.0.0.1:8737, and until now the user had to start that server by hand.
    // That quietly contradicts "double-click and it works" — and if the server
    // ever went away (crash, logout, a stray `kill`) the HUD just sat on
    // "开始启动…" forever, with nothing on screen saying why.
    //
    // So the app now starts the server itself whenever the port is not
    // answering. The binary is looked for in the app bundle first (the packaged
    // layout) and then at the usual install path (a checkout where only
    // `cargo build --release` has been run).

    var serverProc: Process?
    /// Rate limit, so a backend that cannot start is not respawned in a tight
    /// loop by the 1 s poll.
    var lastSpawnAttempt: TimeInterval = 0

    func serverBinaryCandidates() -> [URL] {
        var out: [URL] = []
        if let u = Bundle.main.url(forResource: "tokendance-server", withExtension: nil) {
            out.append(u)
        }
        let home = FileManager.default.homeDirectoryForCurrentUser
        out.append(home.appendingPathComponent(".local/bin/tokendance-server"))
        out.append(URL(fileURLWithPath: "/usr/local/bin/tokendance-server"))
        return out
    }

    func ensureServerRunning() {
        if let p = serverProc, p.isRunning { return }
        let now = CACurrentMediaTime()
        guard now - lastSpawnAttempt > 15 else { return }
        lastSpawnAttempt = now
        guard let bin = serverBinaryCandidates().first(where: {
            FileManager.default.isExecutableFile(atPath: $0.path)
        }) else {
            statusItem.button?.title = L(" 未找到服务端", " no server")
            return
        }
        let p = Process()
        p.executableURL = bin
        p.arguments = ["--port", "8737"]
        // 名单（registry）默认走同一个服务：服务端自己的默认值是 127.0.0.1:8901，
        // 那在用户机器上不存在，于是"更新名单"永远失败。这里把它对齐到 telURL。
        p.environment = ProcessInfo.processInfo.environment
            .merging(["TOKENDANCE_REGISTRY_URL": telURL() + "/registry.json"]) { _, new in new }
        // Append to the server's own log rather than discarding it: a backend
        // that refuses to start is otherwise invisible.
        let home = FileManager.default.homeDirectoryForCurrentUser
        let logURL = home.appendingPathComponent(".tokendance/server.log")
        FileManager.default.createFile(atPath: logURL.path, contents: nil)
        if let fh = try? FileHandle(forWritingTo: logURL) {
            fh.seekToEndOfFile()
            p.standardOutput = fh
            p.standardError = fh
        }
        do {
            try p.run()
            serverProc = p
        } catch {
            NSLog("TokenDance: could not start backend: \(error)")
        }
    }

    func pollLive() {
        URLSession.shared.dataTask(with: liveApi) { [weak self] data, _, _ in
            guard let self else { return }
            guard let data,
                  let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
                // connection refused / malformed — the one case where there is
                // nothing on screen and nothing to show: make sure the backend
                // is actually up
                DispatchQueue.main.async { self.ensureServerRunning() }
                return
            }
            DispatchQueue.main.async { self.applyLive(obj) }
        }.resume()
    }

    // adaptive heat: ratio of current burn to a slow EMA baseline (~2min)
    // — color reflects *relative* intensity, self-calibrating to any workload
    var burnBaseline: Double = -1
    var lastHeat: Double = 0     // target heat (1s poll)
    var dispHeat: Double = 0     // eased heat (frame-level, drives color)
    var lastBurst = false

    func updateHeat(_ v: Int) -> Double {
        if burnBaseline < 0 { burnBaseline = Double(v) }
        burnBaseline += (Double(v) - burnBaseline) / 120.0
        return burnBaseline > 1 ? Double(v) / burnBaseline : 0
    }

    func burnColor(_ heat: Double) -> NSColor {
        // the whole ramp shifts with the surface, not just the idle end: the
        // dark-glass palette is luminous (it has to glow against black), the
        // light-glass one is deepened (a luminous green on white is unreadable)
        let stops: [(v: Double, r: Double, g: Double, b: Double)] = isDarkUI ? [
            (0.00, 0.88, 0.90, 0.94),   // idle: near-white
            (0.15, 0.38, 0.95, 0.62),   // green
            (0.45, 1.00, 0.72, 0.25),   // orange
            (0.75, 1.00, 0.52, 0.32),   // red-orange
            (1.00, 1.00, 0.45, 0.38),   // hot red, still bright
        ] : [
            (0.00, 0.16, 0.18, 0.22),   // idle: near-black
            (0.15, 0.10, 0.60, 0.32),   // deep green
            (0.45, 0.86, 0.46, 0.03),   // amber
            (0.75, 0.84, 0.24, 0.11),   // burnt orange
            (1.00, 0.76, 0.09, 0.11),   // hot red
        ]
        let x = min(max(heat, 0), 1)
        if x <= stops[0].v { return NSColor(red: stops[0].r, green: stops[0].g, blue: stops[0].b, alpha: 1) }
        for i in 1..<stops.count {
            if x <= stops[i].v {
                let a = stops[i-1], b = stops[i]
                let t = (x - a.v) / (b.v - a.v)
                return NSColor(red: a.r + (b.r - a.r) * t,
                               green: a.g + (b.g - a.g) * t,
                               blue: a.b + (b.b - a.b) * t, alpha: 1)
            }
        }
        return NSColor(red: 1.0, green: 0.12, blue: 0.10, alpha: 1)
    }

    // MARK: - i18n (HUD) + appearance
    // Both preferences come from the shared server settings (polled via
    // /api/live), so a change made on the settings page lands here within ~1s.

    /// "sys" (default) | "zh" | "en"
    var langPref: String { UserDefaults.standard.string(forKey: "tb_lang") ?? "sys" }
    var lang: String {
        let p = langPref
        if p == "zh" || p == "en" { return p }
        return Locale.preferredLanguages.first?.hasPrefix("zh") == false ? "en" : "zh"
    }
    func L(_ zh: String, _ en: String) -> String { lang == "en" ? en : zh }

    /// "auto" (default) | "light" | "dark"
    var themePref: String { UserDefaults.standard.string(forKey: "tb_theme") ?? "auto" }

    /// True when the HUD should render on a dark surface.
    ///
    /// Derived straight from the preference + the *app-level* appearance
    /// rather than from `hudPanel.effectiveAppearance`: reading the panel we
    /// are about to re-configure would make the first frame after a switch
    /// disagree with the theme we just applied (and would oscillate on the
    /// explicit light/dark overrides).
    var isDarkUI: Bool {
        switch themePref {
        case "light": return false
        case "dark":  return true
        default:      return NSApp.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        }
    }

    /// The appearance the panel should adopt. `nil` means "inherit", which is
    /// exactly what `auto` needs: the glass then tracks the system setting.
    func resolvedAppearance() -> NSAppearance? {
        switch themePref {
        case "light": return NSAppearance(named: .aqua)
        case "dark":  return NSAppearance(named: .darkAqua)
        default:      return nil
        }
    }

    /// Push the current theme into the vibrancy stack.
    ///
    /// Text and icons need no work here: they are bound to *dynamic* AppKit
    /// colours (`labelColor` / `secondaryLabelColor`), which AppKit re-resolves
    /// against the panel's effective appearance at draw time. Only the
    /// layer-backed snapshots (CGColor) and the material itself are frozen at
    /// assignment, so those are the ones we repaint.
    func applyThemeToHUD(force: Bool = false) {
        guard let panel = hudPanel, let effect = hudEffect else { return }
        let dark = isDarkUI
        let tp = themePref
        if !force, appliedDark == dark, appliedTheme == tp { return }
        appliedDark = dark
        appliedTheme = tp

        // 1) one switch flips every dynamic colour + the vibrancy rendering
        panel.appearance = resolvedAppearance()

        // 2) material + tint. `.hudWindow` in *both* appearances, because it is
        //    the translucent one: it is what a menu bar HUD is, and it adapts.
        //    `.sidebar` was the light-mode choice and it is the reason the light
        //    HUD read as an opaque grey card — that material is a heavy frost
        //    (rendered over colour bands, the bands came through at maybe a
        //    fifth of their saturation). Measured side by side over the same
        //    backdrop, `.hudWindow` plus the tint below is the pair that still
        //    looks like glass while keeping the surface light enough for the
        //    dark text in light mode.
        //
        //    The tint is what stops the surface from following an unhelpful
        //    backdrop: this panel floats over whatever is behind it, including
        //    a black terminal, and the numbers have to stay readable there. It
        //    is a floor on the surface, not a wallpaper-hiding panel — 0.25 of
        //    white leaves plenty of the backdrop's colour and texture visible.
        //
        //    The 1 pt border is the pane's edge. It is the cue that survives a
        //    flat backdrop: over a dark desktop a dark HUD has nothing to show
        //    through, and the highlight is what keeps it reading as a sheet of
        //    glass instead of a dark rectangle.
        effect.material = .hudWindow
        effect.blendingMode = .behindWindow
        effect.layer?.backgroundColor = (dark
            ? NSColor.black.withAlphaComponent(0.22)
            : NSColor.white.withAlphaComponent(0.25)).cgColor
        effect.layer?.borderWidth = 1
        effect.layer?.borderColor = (dark ? NSColor.white.withAlphaComponent(0.12)
                                          : NSColor.white.withAlphaComponent(0.20)).cgColor

        // 3) layer-backed snapshots don't follow dynamic colours — repaint
        repaintLayerChrome(dark: dark)
        // 4) these two carry hand-mixed palette colours rather than dynamic
        //    ones, so refresh them now instead of 80ms later in animStep
        hudBurn?.textColor = burnColor(dispHeat)
        hudStatus?.textColor = lastBurst ? burnColor(dispHeat) : .secondaryLabelColor
        hudRank?.textColor = dark ? NSColor(red: 0.72, green: 0.62, blue: 1.0, alpha: 1)
                                  : NSColor(red: 0.45, green: 0.30, blue: 0.92, alpha: 1)

        hudSide?.isDark = dark
        hudSide?.needsDisplay = true
        // 更新标记是图层实心色，也得按当前外观重取一次
        syncUpdateBadge()

        // the guards in animStep() skip colour writes while nothing changes, so
        // a theme switch has to invalidate them explicitly
        shownHeatBucket = Int.min
        shownBurst = !lastBurst
    }

    /// The three views whose colour lives in a CGColor snapshot rather than in
    /// a dynamic NSColor. Called on theme changes and on every heat repaint.
    func repaintLayerChrome(dark: Bool) {
        hudBarTrack?.layer?.backgroundColor = (dark
            ? NSColor.white.withAlphaComponent(0.12)
            : NSColor.black.withAlphaComponent(0.10)).cgColor
        hudBarFill?.layer?.backgroundColor = (dispHeat > 0.02
            ? burnColor(dispHeat)
            : (dark ? NSColor(white: 0.45, alpha: 1)
                    : NSColor(white: 0.78, alpha: 1))).cgColor
    }

    /// Everything that must react when the shared preferences change (either
    /// locally or because the settings page wrote them on the server).
    func applyLocalPrefs() {
        applyThemeToHUD(force: true)
        statusItem.menu = buildMenu()
        // force the HUD copy onto the new language without waiting for the
        // next poll's text assignment to land
        hudRate?.stringValue = ""
        hudStatus?.stringValue = L("连接中…", "Connecting…")
        hudTodayCap?.stringValue = L("今日已消耗", "Consumed today")
    }

    func fmt(_ n: Int) -> String {
        if lang == "en" {
            if n >= 1_000_000_000_000 { return String(format: "%.2fT", Double(n) / 1e12) }
            if n >= 1_000_000_000 { return String(format: "%.2fB", Double(n) / 1e9) }
            if n >= 1_000_000 { return String(format: "%.1fM", Double(n) / 1e6) }
            if n >= 1_000 { return String(format: "%.1fK", Double(n) / 1e3) }
        } else {
            // 万亿, not 亿, at the top: "90000001.23亿" is eleven glyphs of
            // magnitude in a 158 pt column (STATUS.md 56).
            if n >= 1_000_000_000_000 { return String(format: "%.2f万亿", Double(n) / 1e12) }
            if n >= 100_000_000 { return String(format: "%.2f亿", Double(n) / 1e8) }
            if n >= 10_000 { return String(format: "%.1f万", Double(n) / 1e4) }
        }
        return "\(n)"
    }

    let groupedFmt: NumberFormatter = {
        let f = NumberFormatter()
        f.numberStyle = .decimal
        return f
    }()

    func fmtGrouped(_ n: Int) -> String {
        return groupedFmt.string(from: NSNumber(value: n)) ?? "\(n)"
    }

    /// `Int(_:)` from a `Double` is a *trap*, not a nil or a saturation: NaN,
    /// ±inf and anything past `Int.max` abort the process outright. These
    /// numbers arrive from the server as JSON, so the only way to get one is a
    /// malformed or truncated response — but the failure mode is the whole menu
    /// bar app dying, and a saturated value is indistinguishable from "very
    /// large" for a token counter regardless.
    func safeInt(_ d: Double) -> Int {
        guard d.isFinite else { return 0 }
        return Int(min(max(d, 0), 9e15))
    }

    func applyLive(_ obj: [String: Any]) {
        // ---- shared preferences (lang / theme) ride along with the live poll,
        // so a change made on the settings page reaches the native app within
        // ~1s without either side knowing about the other ----
        syncSharedPrefs(obj["settings"] as? [String: Any])

        let b60 = (obj["burn60"] as? NSNumber)?.intValue ?? 0
        let b10 = (obj["burn10"] as? NSNumber)?.intValue ?? 0
        let active = (obj["active"] as? NSNumber)?.intValue ?? 0
        // null means "no record at all", which is not the same as "stale":
        // the HUD has different words for a fresh machine and for an idle one.
        let agoNum = obj["last_ago_s"] as? NSNumber
        let ago = agoNum?.doubleValue ?? 999
        sawActivity = agoNum != nil
        let today = obj["today"] as? [String: Any] ?? [:]
        let ti = (today["i"] as? NSNumber)?.intValue ?? 0
        let to = (today["o"] as? NSNumber)?.intValue ?? 0
        let tc = (today["c"] as? NSNumber)?.intValue ?? 0
        // The big number is input + output — "total tokens" as every other
        // usage page means it. It used to be `i` alone, which reads like a
        // total but is the prompt side only (and with caching, ~99.8% of it is
        // cache reads). `ti` still drives the hit rate below: that is c/i.
        // `satAdd`, not `+`: this fallback runs on whatever the socket said, and
        // an overflow here is a trap, not a wrong number (STATUS.md 58).
        let tTot = (today["t"] as? NSNumber)?.intValue ?? satAdd(ti, to)

        // Per-agent "today" rides along with this payload, so the rows at the
        // bottom of the HUD follow the same 1 s cadence as everything else on
        // it. They used to come only from the local aggregate, which recomputes
        // every 60 s: after midnight — or the first call of a new day by a
        // source that had no activity yet — the rows could stay missing for up
        // to a minute, which reads as "sometimes they are just not there".
        // Absent (older server) → keep whatever the 60 s aggregate last set.
        if let per = obj["today_agents"] as? [[Int]] {
            var next = Array(repeating: [0, 0, 0, 0], count: agentNames.count)
            for (idx, row) in per.enumerated() where idx < next.count && row.count >= 4 {
                next[idx] = Array(row.prefix(4))
            }
            todayPerAgent = next
        }

        // ---- cat speed: capped low — macOS 15+ yanks status items that
        // update their content too frequently (image swaps > ~6/s) ----
        let fps = b10 > 0 ? min(6.0, 2.0 + Double(b10) / 3000.0) : 0.0
        frameInterval = fps > 0 ? 1.0 / fps : 0.0

        // menu bar: icon only (space is scarce); numbers live in the HUD
        if statusItem.button?.title.isEmpty == false { statusItem.button?.title = "" }

        // HUD big number = PRECISE landed today total (fact; odometer rolls on
        // landing). In-flight extrapolation lives in the small "+N" tail below.
        lastTi = Double(tTot)
        lastB60 = Double(b60)
        lastAgo = ago
        lastLiveAt = CACurrentMediaTime()
        let ext = (b60 > 0 && ago < 20) ? Double(b60) / 60.0 * ago : 0
        // safeInt, not `Int(ext)`: `Int(_:)` from a Double traps on NaN, ±inf and
        // anything past Int.max, and `ext` is derived from a number off the wire.
        hudEst?.stringValue = ext >= 1 ? L("＋估算 ", "+est ") + fmtGrouped(safeInt(ext)) : ""
        hudEst?.isHidden = hudCompact || ext < 1
        let heat = updateHeat(b60)
        lastHeat = heat
        lastBurst = b10 > 0 && b60 > 0
        useBike = useBike ? (heat > 0.40) : (heat > 0.55)  // gait hysteresis
        // color + bar are driven from animStep via dispHeat easing —
        // never set them directly here or they hard-switch every second

        let burst = b10 > 0 && b60 > 0
        if burst {
            // 只留"每分钟烧多少"这一个速率。这里原来还挂着 `· 10s N`（最近 10 秒的
            // 用量），用户看不懂也不需要用——它跟 `/min` 表达的是同一件事的两个时间窗，
            // 而且这一行本身是挂件宽度的主要来源（STATUS 56）。`b10` 仍然参与
            // `lastBurst`（决定跑步小人的冲刺动作），只是不再上屏。
            hudStatus?.stringValue = L("🔥 燃烧中 · \(active) agent · \(fmtGrouped(b60))/min",
                                       "🔥 Burning · \(active) agents · \(fmtGrouped(b60))/min")
        } else if !sawActivity {
            // The one state where the useful thing to say is what to do next.
            hudStatus?.stringValue = L("还没有数据 · 运行 AI 编码工具后自动出现",
                                       "No data yet · appears once an AI tool runs")
        } else if ago < 3600 {
            hudStatus?.stringValue = String(format: L("冷却中 · 最后活动 %.0f 秒前", "Cooling · last active %.0fs ago"), ago)
        } else {
            hudStatus?.stringValue = L("待机 · 今日无活动", "Idle · no activity today")
        }
        // status text color & bar easing happen in animStep (frame-level)

        hudRate?.stringValue = L("今日命中率 \(rate(tc, ti)) · 燃烧 \(fmt(b60))/min",
                                 "Hit rate \(rate(tc, ti)) · burn \(fmt(b60))/min")
        // top-3 agents by today's input tokens; rows re-bound in place
        let ranking = agentNames.indices
            .filter { todayPerAgent[$0][1] > 0 }
            .sorted { todayPerAgent[$0][1] > todayPerAgent[$1][1] }
            .prefix(3)
        for (i, row) in hudRows.enumerated() {
            if i < ranking.count {
                let a = ranking[i]
                let t = todayPerAgent[a]
                row.row.isHidden = hudCompact
                row.dot.layer?.backgroundColor = agentColors[a % agentColors.count].cgColor
                row.name.stringValue = agentNames[a]
                row.val.stringValue = "\(t[0])\(L("次", " calls")) · \(fmt(t[1])) · \(rate(t[3], t[1]))"
            } else {
                row.row.isHidden = true
            }
        }
        // rows/est appear and disappear with activity — keep the panel snug,
        // and re-anchor: content-driven width changes can drift the origin
        // (applyThemeToHUD is a no-op unless the effective theme moved on —
        //  this also catches a system appearance flip if KVO ever misses it)
        applyThemeToHUD()
        // A drag ends on mouse-up, and that is the normal path. But if a
        // mouse-up were ever swallowed while the window was being moved by the
        // system, the panel would stay frozen in place for good — so this also
        // settles the gesture as soon as the button is up.
        if hudDragging, NSEvent.pressedMouseButtons & 0x1 == 0 {
            (hudPanel?.contentView as? DragThroughView)?.settle()
        }
        resizeHUDToFit(animated: false)
        positionHUD()
    }

    /// Mirror the server-side preferences into our local defaults.
    /// The server is the single source of truth for both UIs; whichever side
    /// writes, both converge here.
    func syncSharedPrefs(_ settings: [String: Any]?) {
        guard let settings else { return }
        let newLang = (settings["lang"] as? String) ?? "sys"
        let newTheme = (settings["theme"] as? String) ?? "auto"
        let d = UserDefaults.standard
        var changed = false
        if (d.string(forKey: "tb_lang") ?? "sys") != newLang {
            d.set(newLang, forKey: "tb_lang")
            changed = true
        }
        if (d.string(forKey: "tb_theme") ?? "auto") != newTheme {
            d.set(newTheme, forKey: "tb_theme")
            changed = true
        }
        if changed { applyLocalPrefs() }
    }

    lazy var todayPerAgent: [[Int]] = Array(repeating: Array(repeating: 0, count: 4), count: agentNames.count)

    /// Day stamp for "same calendar day" comparisons.
    ///
    /// Built once, on purpose: constructing a `DateFormatter` costs ~100µs, and
    /// the aggregate loop used to do it *per record* (~1.7s on 25k records).
    /// The hot loop no longer formats at all — see `aggregate()` — this is only
    /// for one-off uses like the leaderboard's `day` field.
    private lazy var dayFmt: DateFormatter = {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd"
        return f
    }()

    func dayKey(_ ms: Double) -> String {
        dayFmt.string(from: Date(timeIntervalSince1970: ms / 1000))
    }

    /// Midnight (local) of the current day, in the same millisecond epoch the
    /// records use — lets the aggregate loop bucket by numeric comparison
    /// instead of formatting a date per record.
    private var startOfTodayMs: Double {
        Calendar.current.startOfDay(for: Date()).timeIntervalSince1970 * 1000
    }

    // Memoised aggregate. Both the right-click menu and the 60s refresh call
    // this, and a full pass is O(records), so the result is kept until the
    // record set is replaced (aggGen) or the day rolls over (aggDayStart).
    private var aggGen = 0            // bumped in refresh() whenever records change
    /// Failed /api/data attempts in a row; see the retry in `refresh()`.
    private var refreshRetry = 0
    private var aggCache: (today: [[Int]], todayTotal: [Int], all: [[Int]], allTotal: [Int])?
    private var aggCacheGen = -1
    private var aggDayStart: Double = -1

    func aggregate() -> (today: [[Int]], todayTotal: [Int], all: [[Int]], allTotal: [Int]) {
        let dayStart = startOfTodayMs
        if aggCacheGen == aggGen, aggDayStart == dayStart, let cached = aggCache { return cached }

        var tAgent = Array(repeating: [0, 0, 0, 0], count: agentNames.count)
        var aAgent = Array(repeating: [0, 0, 0, 0], count: agentNames.count)
        for r in records {
            // `a >= 0` is not redundant with `a < count`: a negative id passes a
            // one-sided bound and then traps on `aAgent[a]`, and array
            // subscripts are not bounds-checked into a graceful failure — they
            // abort the process. The server sends `a` as a `u8` so this cannot
            // happen today, but the client parses whatever is on the socket.
            guard let a = r["a"] as? Int, a >= 0, a < agentNames.count,
                  let ms = (r["t"] as? NSNumber)?.doubleValue,
                  let i0 = (r["i"] as? NSNumber)?.intValue,
                  let o0 = (r["o"] as? NSNumber)?.intValue,
                  let c0 = (r["c"] as? NSNumber)?.intValue else { continue }
            // Clamp like the server does, and add saturating: between them a
            // single absurd field can neither overflow nor crash the process.
            let i = min(max(i0, 0), maxRecordTokens)
            let o = min(max(o0, 0), maxRecordTokens)
            let c = min(max(c0, 0), maxRecordTokens)
            aAgent[a][0] = satAdd(aAgent[a][0], 1)
            aAgent[a][1] = satAdd(aAgent[a][1], i)
            aAgent[a][2] = satAdd(aAgent[a][2], o)
            aAgent[a][3] = satAdd(aAgent[a][3], c)
            if ms >= dayStart {
                tAgent[a][0] = satAdd(tAgent[a][0], 1)
                tAgent[a][1] = satAdd(tAgent[a][1], i)
                tAgent[a][2] = satAdd(tAgent[a][2], o)
                tAgent[a][3] = satAdd(tAgent[a][3], c)
            }
        }
        var tTotal = Array(repeating: 0, count: 4)
        var aTotal = Array(repeating: 0, count: 4)
        for k in 0..<4 {
            for a in 0..<agentNames.count {
                tTotal[k] = satAdd(tTotal[k], tAgent[a][k])
                aTotal[k] = satAdd(aTotal[k], aAgent[a][k])
            }
        }
        let result = (tAgent, tTotal, aAgent, aTotal)
        aggCache = result
        aggCacheGen = aggGen
        aggDayStart = dayStart
        return result
    }

    func rate(_ c: Int, _ i: Int) -> String {
        i == 0 ? "-" : String(format: "%.1f%%", Double(c) / Double(i) * 100)
    }

    // MARK: - UI updates

    func updateMenuAndTitle() {
        let agg = aggregate()
        todayPerAgent = agg.today
        // No title is set here on purpose: the status item is a fixed 34 pt and
        // the number lives in the HUD. This used to be gated on `burn60 <= 0`,
        // but nothing in the app ever assigned that field, so the gate was
        // always true and every 60 s refresh wrote " <total>" into the menu bar
        // — which `applyLive` then wiped on its next 1 s tick. Net effect: the
        // token total flashed in the menu bar for under a second, sixty
        // seconds apart.
        statusItem.menu = buildMenu()
    }

    /// A real main menu, now that the app is a regular app rather than an
    /// accessory. Without one the app menu is empty and ⌘Q / ⌘H / ⌘C / ⌘V do
    /// nothing, because those key equivalents belong to menu items, not to the
    /// app itself. The Edit menu is not decoration either: the dashboard is a
    /// WKWebView with a text field in it, and it swallows ⌘C/⌘V unless the menu
    /// advertises them.
    func installMainMenu() {
        let main = NSMenu()

        // The system replaces this item's title with the app's own name, so
        // the string here is only a placeholder.
        let appItem = NSMenuItem()
        main.addItem(appItem)
        let appMenu = NSMenu(title: "TokenDance")
        appItem.submenu = appMenu
        let about = NSMenuItem(title: L("关于 TokenDance", "About TokenDance"),
                               action: #selector(showAbout), keyEquivalent: "")
        about.target = self
        appMenu.addItem(about)
        appMenu.addItem(.separator())
        appMenu.addItem(NSMenuItem(title: L("隐藏 TokenDance", "Hide TokenDance"),
                                   action: #selector(NSApplication.hide(_:)), keyEquivalent: "h"))
        let hideOthers = NSMenuItem(title: L("隐藏其他", "Hide Others"),
                                    action: #selector(NSApplication.hideOtherApplications(_:)),
                                    keyEquivalent: "h")
        hideOthers.keyEquivalentModifierMask = [.command, .option]
        appMenu.addItem(hideOthers)
        appMenu.addItem(NSMenuItem(title: L("全部显示", "Show All"),
                                   action: #selector(NSApplication.unhideAllApplications(_:)),
                                   keyEquivalent: ""))
        appMenu.addItem(.separator())
        appMenu.addItem(NSMenuItem(title: L("退出 TokenDance", "Quit TokenDance"),
                                   action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"))

        let editItem = NSMenuItem()
        main.addItem(editItem)
        let editMenu = NSMenu(title: L("编辑", "Edit"))
        editItem.submenu = editMenu
        func add(_ zh: String, _ en: String, _ sel: Selector, _ key: String,
                 _ mods: NSEvent.ModifierFlags = [.command]) {
            let it = NSMenuItem(title: L(zh, en), action: sel, keyEquivalent: key)
            it.keyEquivalentModifierMask = mods
            editMenu.addItem(it)
        }
        add("撤销", "Undo", Selector(("undo:")), "z")
        add("重做", "Redo", Selector(("redo:")), "z", [.command, .shift])
        editMenu.addItem(.separator())
        add("剪切", "Cut", #selector(NSText.cut(_:)), "x")
        add("拷贝", "Copy", #selector(NSText.copy(_:)), "c")
        add("粘贴", "Paste", #selector(NSText.paste(_:)), "v")
        add("全选", "Select All", #selector(NSText.selectAll(_:)), "a")

        NSApp.mainMenu = main
    }

    func buildMenu() -> NSMenu {
        let menu = NSMenu()
        menu.autoenablesItems = false
        let agg = aggregate()

        func addLine(_ s: String) { let it = NSMenuItem(title: s, action: nil, keyEquivalent: ""); it.isEnabled = false; menu.addItem(it) }

        if records.isEmpty {
            addLine(L("⚠️ 未获取到数据（检查 8737 服务）", "⚠️ No data yet (check the 8737 service)"))
        } else {
            addLine(L("今天 · 全部 Agent", "Today · All Agents"))
            addLine(L("调用 \(agg.todayTotal[0]) 次 · 输入 \(fmt(agg.todayTotal[1])) · 输出 \(fmt(agg.todayTotal[2]))",
                      "Calls \(agg.todayTotal[0]) · in \(fmt(agg.todayTotal[1])) · out \(fmt(agg.todayTotal[2]))"))
            addLine(L("缓存命中率 \(rate(agg.todayTotal[3], agg.todayTotal[1]))",
                      "Cache hit rate \(rate(agg.todayTotal[3], agg.todayTotal[1]))"))
            menu.addItem(.separator())
            for a in 0..<agentNames.count {
                let t = agg.today[a]
                if t[0] == 0 { continue }
                addLine(L("\(agentNames[a])·今日: \(t[0])次 · in \(fmt(t[1])) · 命中 \(rate(t[3], t[1]))",
                          "\(agentNames[a])·today: \(t[0]) calls · in \(fmt(t[1])) · hit \(rate(t[3], t[1]))"))
            }
            menu.addItem(.separator())
            for a in 0..<agentNames.count {
                let v = agg.all[a]
                if v[0] == 0 { continue }
                addLine(L("\(agentNames[a])·累计: \(v[0])次 · in \(fmt(v[1])) · out \(fmt(v[2])) · 命中 \(rate(v[3], v[1]))",
                          "\(agentNames[a])·total: \(v[0]) calls · in \(fmt(v[1])) · out \(fmt(v[2])) · hit \(rate(v[3], v[1]))"))
            }
        }
        menu.addItem(.separator())
        let dash = NSMenuItem(title: L("打开完整仪表盘", "Open Full Dashboard"), action: #selector(openDashboard), keyEquivalent: "d")
        dash.target = self
        menu.addItem(dash)
        let hud = NSMenuItem(title: L("显示悬浮窗", "Show Floating HUD"), action: #selector(toggleHUD), keyEquivalent: "h")
        hud.target = self
        hud.state = hudVisible ? .on : .off
        menu.addItem(hud)
        let fold = NSMenuItem(title: hudCompact ? L("展开悬浮窗", "Expand HUD") : L("收起悬浮窗（只留数字）", "Collapse HUD (number only)"),
                              action: #selector(toggleCompact), keyEquivalent: "")
        fold.target = self
        menu.addItem(fold)
        let dock = NSMenuItem(title: hudDocked ? L("贴边小圆 → 展开面板", "Bubble → Restore Panel") : L("贴边收起（小圆 + 燃烧光环）", "Dock to Side (bubble + burn ring)"),
                              action: #selector(toggleDock), keyEquivalent: "")
        dock.target = self
        menu.addItem(dock)
        // Only offered once the panel has actually been moved: there is no
        // window list to find the panel in, so without a way back a panel parked
        // in an awkward spot would be stuck there.
        if hudUserMoved {
            let reset = NSMenuItem(title: L("悬浮窗回到右上角", "Reset HUD Position"),
                                   action: #selector(resetHUDPosition), keyEquivalent: "")
            reset.target = self
            menu.addItem(reset)
        }
        let rf = NSMenuItem(title: L("刷新数据", "Refresh Data"), action: #selector(refreshNow), keyEquivalent: "r")
        rf.target = self
        menu.addItem(rf)
        let lbItem = NSMenuItem(title: lbEnabled() ? L("退出全球排行榜", "Leave Global Leaderboard") : L("加入全球排行榜（匿名）", "Join Global Leaderboard (anonymous)"),
                                action: #selector(toggleLB), keyEquivalent: "")
        lbItem.target = self
        menu.addItem(lbItem)
        if lbEnabled() {
            // 范围只影响"你看到的排名"，不影响上报（每天照报当日用量）。
            let rangeItem = NSMenuItem(title: L("排名范围：", "Ranking range: ") + lbRangeLabel(),
                                       action: nil, keyEquivalent: "")
            let sub = NSMenu()
            for (d, zh, en) in [(1, "今天", "Today"), (7, "近 7 天", "Last 7d"),
                                (30, "近 30 天", "Last 30d"), (0, "全部", "All time")] {
                let it = NSMenuItem(title: L(zh, en), action: #selector(setLbDays(_:)), keyEquivalent: "")
                it.target = self
                it.tag = d
                it.state = (lbDays() == d) ? .on : .off
                sub.addItem(it)
            }
            rangeItem.submenu = sub
            menu.addItem(rangeItem)
        }
        let telItem = NSMenuItem(title: telEnabled() ? L("关闭匿名使用统计", "Disable Anonymous Usage Stats") : L("开启匿名使用统计（只上报版本/系统/语言等环境信息，不含任何用量内容）", "Enable Anonymous Usage Stats (build, OS and settings only — no usage content)"),
                                 action: #selector(toggleTelemetry), keyEquivalent: "")
        telItem.target = self
        menu.addItem(telItem)
        menu.addItem(.separator())
        if updateTask != nil {
            // 下载/校验/解压过程中：显示阶段与进度，并把"现在更新"置灰
            let item = NSMenuItem(title: "⬇ " + (updatePhase.isEmpty ? L("正在更新…", "Updating…") : updatePhase),
                                  action: nil, keyEquivalent: "")
            item.isEnabled = false
            menu.addItem(item)
        } else if let u = updateInfo {
            // 有新版本：点这一条就开始更新（图标上也有橙点提示）
            let item = NSMenuItem(title: L("⬇ 有新版本 \(u.version) · 现在更新", "⬇ Version \(u.version) available · update now"),
                                  action: #selector(startUpdate), keyEquivalent: "")
            item.target = self
            menu.addItem(item)
            if !updatePhase.isEmpty {          // 上一次失败的原因留在菜单里
                let why = NSMenuItem(title: updatePhase, action: nil, keyEquivalent: "")
                why.isEnabled = false
                menu.addItem(why)
            }
            let page = NSMenuItem(title: L("打开发布页", "Open the release page"),
                                  action: #selector(openUpdateURL), keyEquivalent: "")
            page.target = self
            menu.addItem(page)
        }
        let upd = NSMenuItem(title: L("检查更新", "Check for Updates"), action: #selector(checkUpdateManually), keyEquivalent: "u")
        upd.target = self
        menu.addItem(upd)
        menu.addItem(.separator())
        let quit = NSMenuItem(title: L("退出", "Quit"), action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        menu.addItem(quit)
        return menu
    }

    // MARK: - 后端地址与更新状态（行为在 Telemetry.swift / Updater.swift）
    /// 埋点 / 更新检查 / 排行榜 / 名单，全在这个地址上。
    ///
    /// **默认必须是生产地址**：早先这里写的是 `http://127.0.0.1:8901`（开发时本机
    /// 容器的地址），在新用户的机器上那个端口根本没人监听——埋点、更新检查、排行榜、
    /// 名单更新会一起静默失效，而且因为全是"网络失败"，界面上看不出原因。
    /// 开发时用 `defaults write com.tokendance.app tb_tel_url http://127.0.0.1:8901`
    /// 指回本机即可。
    ///
    /// 值来自 `ServiceConfig.base`（`ServiceConfig.swift.in` 生成，构建时可用
    /// `SERVICE_BASE=` 覆盖）——**客户端里写死远端地址的地方只有那一处**。
    static let defaultTelURL = ServiceConfig.base
    /// (版本, 发布页路径, 安装包路径)。**`file` 才是安装包**——`url` 是给人看的下载页，
    /// 早先拿 url 去下载，下回来的是 14 KB 的 HTML，靠 sha256 校验才发现（STATUS 76）。
    var updateInfo: (version: String, url: String, file: String)?
    /// 自我更新的运行状态：下载任务、进度、当前阶段文案、期望的 sha256。
    var updateTask: URLSessionDownloadTask?
    var updateSession: URLSession?
    var updateProgress: Double = 0
    var updatePhase: String = ""
    var updateSHA: String?


    /// When this process started, so the ping can report how long it has been up.
    static let launchedAt = Date()


    // MARK: - HUD floating panel

    func makeLabel(_ text: String, size: CGFloat, weight: NSFont.Weight, color: NSColor) -> NSTextField {
        let l = NSTextField(labelWithString: text)
        l.font = .systemFont(ofSize: size, weight: weight)
        l.textColor = color
        l.lineBreakMode = .byTruncatingTail
        return l
    }

    func showHUD() {
        if hudPanel == nil {
            let p = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 292, height: 226),
                            styleMask: [.borderless, .nonactivatingPanel],
                            backing: .buffered, defer: false)
            p.level = .floating
            p.isOpaque = false
            p.backgroundColor = .clear
            p.hasShadow = true
            // Dragging is started by DragThroughView (`mouseDown` →
            // `performDrag(with:)`), which is why the window's own background
            // drag stays off: it would compete for the same gesture. `isMovable`
            // has to be on for the system drag to accept the window at all —
            // it defaults to false for a borderless panel, and without it
            // `performDrag` returns having moved nothing.
            p.isMovable = true
            p.isMovableByWindowBackground = false
            p.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
            // appearance is owned by applyThemeToHUD() — it honours the
            // lang/theme preference shared with the web UI (auto = inherit)

            let content = DragThroughView(frame: p.contentView?.bounds ?? p.frame)
            content.autoresizingMask = [.width, .height]
            content.menuHandler = { TokenDanceApp.delegate.buildMenu() }
            p.contentView = content

            let effect = NSVisualEffectView(frame: content.bounds)
            effect.autoresizingMask = [.width, .height]
            // frosted pane — wallpaper blurs through like glass. These are only
            // the values used for the first frame; applyThemeToHUD() sets the
            // final material/tint pair for the current light/dark appearance,
            // and it runs before the panel is ordered on screen.
            effect.material = .hudWindow
            effect.blendingMode = .behindWindow
            effect.state = .active
            effect.wantsLayer = true
            effect.layer?.cornerRadius = 16
            effect.layer?.masksToBounds = true
            effect.layer?.backgroundColor = NSColor.black.withAlphaComponent(0.22).cgColor
            effect.layer?.borderWidth = 1
            effect.layer?.borderColor = NSColor.white.withAlphaComponent(0.12).cgColor
            content.addSubview(effect)

            let stack = NSStackView()
            stack.orientation = .vertical
            stack.alignment = .leading
            stack.spacing = 5
            stack.translatesAutoresizingMaskIntoConstraints = false
            effect.addSubview(stack)
            hudStack = stack
            hudEffect = effect
            NSLayoutConstraint.activate([
                stack.topAnchor.constraint(equalTo: effect.topAnchor, constant: 14),
                stack.leadingAnchor.constraint(equalTo: effect.leadingAnchor, constant: 16),
                stack.trailingAnchor.constraint(equalTo: effect.trailingAnchor, constant: -16),
                stack.bottomAnchor.constraint(lessThanOrEqualTo: effect.bottomAnchor, constant: -12),
            ])

            // semantic colours only — AppKit re-resolves them against the
            // panel's effective appearance, so light/dark needs no re-tinting
            let title = makeLabel("⚡ TokenDance", size: 10, weight: .semibold, color: .secondaryLabelColor)
            let spacerView = NSView()
            let updateBadge = makeUpdateBadge()
            hudUpdateBadge = updateBadge
            let refreshBtn = symbolButton("arrow.clockwise", fallback: "⟳",
                                          action: #selector(refreshNow))
            let collapseBtn = symbolButton("chevron.down", fallback: "▾",
                                           action: #selector(toggleCompact))
            hudCollapseBtn = collapseBtn
            // was ✕ (hide HUD) — now docks the panel to the screen edge as a
            // small heat-ringed bubble; hide/show lives in the menu
            let closeBtn = symbolButton("arrow.right.to.line", fallback: "⇥",
                                        action: #selector(toggleDock))
            // 更新标记紧跟弹性空白：没有更新时它隐藏，NSStackView 会把位置让出来，
            // 所以这一行在常态下和以前完全一样宽
            let titleRow = NSStackView(views: [title, spacerView, updateBadge,
                                               refreshBtn, collapseBtn, closeBtn])
            titleRow.orientation = .horizontal
            // identical fixed-size Symbol buttons + an explicit shared centre
            // line: this is what actually pins the three icons on one row
            titleRow.alignment = .centerY
            titleRow.spacing = 4
            titleRow.translatesAutoresizingMaskIntoConstraints = false
            // must join the hierarchy BEFORE pinning to stack.widthAnchor —
            // activating a cross-hierarchy constraint throws NSGenericException,
            // which AppKit swallows and silently kills the rest of showHUD
            stack.addArrangedSubview(titleRow)
            hudTitleRow = titleRow
            titleRow.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
            spacerView.setContentHuggingPriority(.init(1), for: .horizontal)
            spacerView.setContentCompressionResistancePriority(.init(1), for: .horizontal)

            let burn = RollingNumberView()
            burn.fontSize = 24
            // 数字**不再限宽**：完整的数字说了算，放不下就由面板变宽来容纳
            // （用户："如果数字长了，窗口宽度就自适应"）。原来的 190 pt 上限是为了
            // "数字再长也不许撑宽面板"，那条口径连同数量级切换一起删掉了。
            // 面板宽度由 `resizeHUDToFit()` 按内容算，能长也能缩（第 56 节那个
            // "只会长不会回"的棘轮问题靠它避免）。
            burn.maxWidth = .greatestFiniteMagnitude
            burn.translatesAutoresizingMaskIntoConstraints = false
            // Caption for the big number. It is input + output for *today*, and
            // nothing on screen said so: the unit label next to it reads
            // "tokens" (or "≈ 2.5亿" in precise mode), which says what kind of
            // thing it is but not which period or which side of a call it
            // counts.
            let todayCap = makeLabel(L("今日已消耗", "Consumed today"),
                                     size: 10, weight: .semibold, color: .tertiaryLabelColor)
            hudTodayCap = todayCap
            let unit = makeLabel("tokens", size: 13, weight: .semibold, color: .labelColor)
            hudUnit = unit
            // compact mode keeps the number plus this always-visible expander,
            // so expanding never depends on gesture hit-testing
            let expandBtn = symbolButton("chevron.right", fallback: "▸",
                                         action: #selector(toggleCompact))
            expandBtn.isHidden = true
            hudExpandBtn = expandBtn
            // 紧凑模式下标题行整行都收起来了，这一行是唯一还看得见的地方
            let compactBadge = makeUpdateBadge()
            hudUpdateBadgeCompact = compactBadge
            let burnRow = NSStackView(views: [burn, unit, compactBadge, expandBtn])
            burnRow.orientation = .horizontal
            burnRow.alignment = .centerY
            // 10, not 6: a 24 pt digit column carries ~4 pt of its own padding,
            // which used to leave the unit label looking glued to the last digit
            // once the compression was gone (measured: 5.5 pt of ink-to-ink).
            burnRow.spacing = Self.hudRowSpacing
            // the caption sits above the number: no extra width (a floating
            // panel that grows sideways covers whatever is behind it), and the
            // reading order is caption → number → estimate
            stack.addArrangedSubview(todayCap)
            stack.addArrangedSubview(burnRow)
            hudBurn = burn

            let est = makeLabel("", size: 11, weight: .semibold, color: .secondaryLabelColor)
            stack.addArrangedSubview(est)
            est.isHidden = true
            hudEst = est

            let status = makeLabel(L("连接中…", "Connecting…"), size: 11, weight: .semibold, color: .secondaryLabelColor)
            stack.addArrangedSubview(status)
            hudStatus = status

            let track = NSView()
            track.wantsLayer = true
            track.translatesAutoresizingMaskIntoConstraints = false
            track.layer?.backgroundColor = NSColor.white.withAlphaComponent(0.12).cgColor
            track.layer?.cornerRadius = 3
            track.heightAnchor.constraint(equalToConstant: 6).isActive = true
            stack.addArrangedSubview(track)
            // 宽度跟着面板走（原来是写死的 260）：数字变长把面板撑宽时，进度条要一起变长，
            // 否则右边会缺一块。约束必须在加入层级之后再激活，跨层级的约束会抛异常
            // 并被 AppKit 吞掉（这个坑 STATUS 里记过）。
            track.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
            hudBarTrack = track

            let fill = NSView()
            fill.wantsLayer = true
            fill.layer?.backgroundColor = NSColor.gray.cgColor
            fill.layer?.cornerRadius = 3
            // The bar stretches via a layer transform, not by moving a constraint:
            // a constraint constant re-runs Auto Layout over the whole HUD (visual
            // effect view + stack view + ring) for every single step, and at the
            // 12.5 Hz the animation timer runs at, that is both expensive and
            // visibly steppy. Scaling the layer costs nothing and can run at 60 Hz
            // — see `smoothStep`. Anchor at the left edge so it grows rightwards
            // (AppKit derives position from frame + anchorPoint, so the layout
            // engine still places it correctly).
            fill.layer?.anchorPoint = CGPoint(x: 0, y: 0.5)
            fill.layer?.transform = CATransform3DMakeScale(0, 1, 1)
            fill.translatesAutoresizingMaskIntoConstraints = false
            track.addSubview(fill)
            NSLayoutConstraint.activate([
                fill.leadingAnchor.constraint(equalTo: track.leadingAnchor),
                fill.topAnchor.constraint(equalTo: track.topAnchor),
                fill.bottomAnchor.constraint(equalTo: track.bottomAnchor),
                fill.heightAnchor.constraint(equalTo: track.heightAnchor),
                fill.widthAnchor.constraint(equalTo: track.widthAnchor),  // full width; the transform scales it
            ])
            hudBarFill = fill

            let rateL = makeLabel(L("今日命中率 -", "Hit rate -"), size: 11, weight: .semibold, color: .labelColor)
            stack.addArrangedSubview(rateL)
            hudRate = rateL

            let rank = makeLabel("", size: 11, weight: .semibold, color: NSColor(red: 0.45, green: 0.30, blue: 0.92, alpha: 1))
            stack.addArrangedSubview(rank)
            rank.isHidden = true   // empty label still reserves height — hide until filled
            hudRank = rank

            // top-3 agent rows — names/dots re-bound in applyLive
            for _ in 0..<3 {
                let dot = NSView()
                dot.wantsLayer = true
                dot.layer?.cornerRadius = 4
                dot.translatesAutoresizingMaskIntoConstraints = false
                dot.widthAnchor.constraint(equalToConstant: 8).isActive = true
                dot.heightAnchor.constraint(equalToConstant: 8).isActive = true

                let name = makeLabel("-", size: 11, weight: .medium, color: .labelColor)
                let val = makeLabel("-", size: 11, weight: .regular, color: .labelColor)
                val.alignment = .right
                val.widthAnchor.constraint(equalToConstant: 158).isActive = true

                // 一个弹性空隙把数值列顶到右边：面板被长数字撑宽时，这一行跟着变宽而不是
                // 挤在左边（宽度没有变化时，效果与原来的左对齐完全一样）
                let gap = NSView()
                gap.setContentHuggingPriority(.init(1), for: .horizontal)
                gap.setContentCompressionResistancePriority(.init(1), for: .horizontal)
                let row = NSStackView(views: [dot, name, gap, val])
                row.orientation = .horizontal
                row.spacing = 7
                row.translatesAutoresizingMaskIntoConstraints = false
                stack.addArrangedSubview(row)
                row.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
                hudRows.append((dot, name, val, row))
            }

            // collapsible sections: compact mode keeps only the rolling number
            hudCollapsible = [hudTitleRow!, todayCap, est, status, track, rateL, rank] + hudRows.map { $0.row }
            // 启动那一刻可能已经查到新版本了（checkUpdate 先跑），补一次状态
            syncUpdateBadge()

            // plain click anywhere on the panel → expand while collapsed,
            // otherwise open the full dashboard (handled in hudClicked)
            content.clickHandler = { [weak self] in self?.hudClicked() }
            // Dragging the panel. Three things have to be true at once and they
            // happen at three different moments:
            //  * on mouse-down the poll must stop re-anchoring the panel — the
            //    window is already following the pointer, and a `setFrameOrigin`
            //    landing mid-drag is what used to make the HUD feel nailed down;
            //  * once the window has actually moved, its position is the user's
            //    to own, and it is remembered across launches;
            //  * on mouse-up that ownership is settled either way, so a click
            //    (window never moved) never silently claims the panel.
            content.dragBeganHandler = { [weak self] in self?.hudDragging = true }
            content.dragSettledHandler = { [weak self] in self?.hudDragging = false }
            content.dragMovedHandler = { [weak self] origin in
                guard let self, !self.hudDocked else { return }
                self.hudUserMoved = true
                self.saveHUDPosition(origin)
            }

            // edge-docked bubble (hidden until "dock to side" is used)
            let side = RadialHUDView(frame: content.bounds)
            side.autoresizingMask = [.width, .height]
            side.isHidden = true
            side.onClick = { [weak self] in self?.toggleDock() }
            side.menuHandler = { [weak self] in self?.buildMenu() ?? NSMenu() }
            content.addSubview(side)
            hudSide = side

            hudPanel = p
        }
        // shrink the borderless panel to hug its content (rank/est start hidden)
        if let panel = hudPanel {
            let t = hudTargetHeight()
            panel.setFrame(NSRect(origin: panel.frame.origin,
                                  size: NSSize(width: panel.frame.width, height: t)), display: false)
        }
        // A position the user dragged to last time is restored *before* the
        // first placement, and marks the panel as user-owned so `positionHUD`
        // only re-checks that it is still reachable instead of resetting it.
        if let p = hudPanel, let saved = savedHUDPosition() {
            hudUserMoved = true
            p.setFrameOrigin(DragThroughView.clampToVisibleUnion(saved, size: p.frame.size))
        }
        // the glass can only be configured once the views exist
        applyThemeToHUD(force: true)
        positionHUD()
        hudPanel?.orderFrontRegardless()
        hudVisible = true
    }

    /// Panel height that hugs the visible content (14pt top, 12pt bottom
    /// padding). Summed manually: NSStackView.fittingSize caches the layout
    /// from the last pass, so right after hiding views it still reports the
    /// pre-hide height and the panel would never shrink.
    private func hudTargetHeight() -> CGFloat {
        guard let stack = hudStack else { return 226 }
        var total: CGFloat = 0
        var counted = 0
        for v in stack.arrangedSubviews {
            // the "+est" line blinks with the live burn rate — its slot stays
            // counted while expanded so the panel never jumps every second
            let keep = !v.isHidden || (!hudCompact && v === hudEst)
            guard keep else { continue }
            if counted > 0 { total += stack.spacing }
            total += max(v.fittingSize.height, v.intrinsicContentSize.height)
            counted += 1
        }
        return min(max(total, 40) + 26, 340)
    }

    /// Font of the little unit label next to the big number — the same one
    /// `makeLabel` builds it with, so measuring here and rendering there agree.
    private static let hudUnitFont = NSFont.systemFont(ofSize: 13, weight: .semibold)

    /// 大数字的规则（2026-09-16 用户定的，简化到只剩两条）：
    ///   * 完整数字，永远不缩写、不按量级退化；
    ///   * 旁边跟一句 "≈ 8.8亿" 的量级参照（中文 万/亿/万亿，英文 K/M/B/T）。
    ///
    /// 数字变长时**不再缩字、也不再换成数量级**——窗口宽度自己跟着变（`resizeHUDToFit`）。
    /// 原来那套"数量级选择"（自动/千/百万/亿/精确）连同它的代码一起删掉了：
    /// 用户不要"按设置换单位"，也不要"太长就自动缩写"，只要真实数字加一个约等于。
    ///
    /// 收起成"只留数字"那个形态时不带估算——那个形态的定义就是只有数字。
    private func hudNumberStyle(_ n: Int) -> (text: String, label: String, labelHidden: Bool) {
        if hudCompact { return (fmtGrouped(n), "tokens", true) }
        // 小数字（< 100 万）给"约等于"没有意义，标签仍写 tokens
        let est = n >= 1_000_000 ? "≈ " + abbrevAuto(n) : "tokens"
        return (fmtGrouped(n), est, false)
    }

    /// Gap between the number and its unit label.
    private static let hudRowSpacing: CGFloat = 10

    /// The automatic magnitude: the largest unit that fits.
    ///
    /// The ladder has to end somewhere: without its top rung a 19-digit value
    /// printed as "112915680.55亿" — eleven glyphs of magnitude, which is both
    /// unreadable and wide enough to stretch the panel. 万亿 / T covers every
    /// value up to 9999 万亿, and past that the number view's own width ceiling
    /// squeezes the glyphs instead of the panel.
    ///
    /// Also used for the docked bubble (~5 glyphs max) — a 76 pt disc cannot
    /// hold "920,031千", so the bubble never follows a pinned level.
    func abbrevAuto(_ n: Int) -> String {
        let v = Double(n)
        if lang == "en" {
            if v >= 1e12 { return String(format: "%.1fT", v / 1e12) }
            if v >= 1e9 { return String(format: "%.1fB", v / 1e9) }
            if v >= 1e6 { return String(format: "%.1fM", v / 1e6) }
            if v >= 1e3 { return String(format: "%.0fK", v / 1e3) }
        } else {
            if v >= 1e12 { return String(format: "%.1f万亿", v / 1e12) }
            if v >= 1e8 { return String(format: "%.1f亿", v / 1e8) }
            if v >= 1e6 { return String(format: "%.0f万", v / 1e4) }   // 100万–9999万
            if v >= 1e4 { return String(format: "%.1f万", v / 1e4) }
        }
        return "\(n)"
    }

    /// Place the HUD.
    ///
    /// Deliberately *not* an enforcement of one position: it is called on
    /// launch, on display changes, after the panel resizes, and on every 1 s
    /// live poll. So it means "make sure the panel is somewhere sane", and a
    /// position the user picked themselves counts as sane.
    ///
    /// But a HUD parked over the top-right corner sits on top of other apps'
    /// toolbars and window controls, which is exactly why the user needs to be
    /// able to move it — and why this must never drag it back.
    private func positionHUD() {
        guard let p = hudPanel else { return }
        // never fight an in-flight collapse/expand animation: a stray
        // setFrameOrigin cancels the animated frame and the panel would
        // appear to ignore the toggle
        guard !hudResizing else { return }
        guard !hudDocked else { return }   // docked: geometry is owned by toggleDock
        // a drag owns the panel's frame from mouse-down to mouse-up; this runs
        // from the 1 s poll, and writing the frame here is what would make the
        // window fight the pointer
        guard !hudDragging else { return }

        if hudUserMoved {
            // the user's placement wins; only rescue it if it has ended up on a
            // screen that is no longer attached
            let fixed = DragThroughView.clampToVisibleUnion(p.frame.origin, size: p.frame.size)
            if fixed != p.frame.origin { p.setFrameOrigin(fixed) }
            return
        }

        let screens = NSScreen.screens
        let screen = screens.first(where: { $0.frame.width > 500 }) ?? screens.first
        guard let screen, screen.frame.width > 500 else { return }
        // `visibleFrame`, not `frame`: the full frame includes the menu bar and
        // the Dock, and on this machine the Dock sits on the *right* — the panel
        // was landing under it (STATUS.md 60).
        let f = screen.visibleFrame
        let x = f.maxX - p.frame.width - 16
        let y = f.maxY - p.frame.height - 8
        guard x > 200 else { return }
        if abs(p.frame.origin.x - x) > 1 || abs(p.frame.origin.y - y) > 1 {
            p.setFrameOrigin(NSPoint(x: x, y: y))
        }
    }

    // MARK: - Remembering where the user put the panel

    private static let hudPosXKey = "tb_hud_x"
    private static let hudPosYKey = "tb_hud_y"

    func savedHUDPosition() -> NSPoint? {
        let d = UserDefaults.standard
        guard d.object(forKey: Self.hudPosXKey) != nil,
              d.object(forKey: Self.hudPosYKey) != nil else { return nil }
        return NSPoint(x: d.double(forKey: Self.hudPosXKey),
                       y: d.double(forKey: Self.hudPosYKey))
    }

    func saveHUDPosition(_ origin: NSPoint) {
        let d = UserDefaults.standard
        d.set(Double(origin.x), forKey: Self.hudPosXKey)
        d.set(Double(origin.y), forKey: Self.hudPosYKey)
    }

    /// Drop the remembered position and put the panel back in its default
    /// corner. Reachable from the right-click menu only once the panel has been
    /// moved — a user who parks it somewhere awkward otherwise has no way back,
    /// since there is no window list to find the panel in.
    @objc func resetHUDPosition() {
        let d = UserDefaults.standard
        d.removeObject(forKey: Self.hudPosXKey)
        d.removeObject(forKey: Self.hudPosYKey)
        hudUserMoved = false
        guard let p = hudPanel, !hudDocked else { return }
        hudResizing = false
        positionHUD()
        hudRestoreFrame = p.frame
        statusItem.menu = buildMenu()
    }

    /// Fixed-size, borderless icon button for the HUD header.
    ///
    /// SF Symbols are used whenever the OS provides them: they are optically
    /// centred inside their box — which is what finally puts the three header
    /// icons on one line — and, being template images, they follow the
    /// light/dark glass through `contentTintColor` with no re-tinting.
    private func symbolButton(_ symbol: String, fallback: String, action: Selector) -> NSButton {
        let b = NSButton(title: "", target: self, action: action)
        b.isBordered = false
        b.bezelStyle = .regularSquare
        b.font = .systemFont(ofSize: 17, weight: .semibold)
        b.contentTintColor = .secondaryLabelColor
        b.imageScaling = .scaleProportionallyDown
        b.translatesAutoresizingMaskIntoConstraints = false
        // exact, identical boxes for every header control, so neither the hit
        // target nor the alignment depends on a glyph's own metrics
        b.widthAnchor.constraint(equalToConstant: 24).isActive = true
        b.heightAnchor.constraint(equalToConstant: 24).isActive = true
        setSymbol(b, symbol, fallback: fallback)
        return b
    }

    /// Swap a button's glyph — used by the expand/collapse pair.
    private func setSymbol(_ b: NSButton, _ symbol: String, fallback: String) {
        if let img = NSImage(systemSymbolName: symbol, accessibilityDescription: nil) {
            let cfg = NSImage.SymbolConfiguration(pointSize: 12.5, weight: .semibold)
            b.image = img.withSymbolConfiguration(cfg) ?? img
            b.imagePosition = .imageOnly
            b.title = ""
        } else {
            // pre-SF-Symbols fallback: the original text glyphs
            b.image = nil
            b.imagePosition = .noImage
            b.title = fallback
        }
    }

    private func setSymbol(_ b: NSButton?, _ symbol: String, fallback: String) {
        guard let b else { return }
        setSymbol(b, symbol, fallback: fallback)
    }

    /// 挂件上的更新标记：**蓝色实心圆 + 白色下载箭头**（用户拿 Codex 那个当样式给的），
    /// 只在真的检测到新版本时出现，点一下就是菜单里的「现在更新」。
    ///
    /// 为什么不是一个共享实例：一个视图只能有一个父视图，而它要同时出现在标题行
    /// （正常模式）和数字行（紧凑模式，那时标题行整行是隐藏的）。所以两处各一个，
    /// 由 `syncUpdateBadge()` 按当前模式决定显示哪个。
    private func makeUpdateBadge() -> NSButton {
        let b = NSButton(title: "", target: self, action: #selector(startUpdate))
        b.isBordered = false
        b.bezelStyle = .regularSquare
        b.imagePosition = .imageOnly
        b.imageScaling = .scaleNone
        b.translatesAutoresizingMaskIntoConstraints = false
        // 18×18 的**画好的图**，而不是"按钮图层涂蓝"：按钮在行里会被纵向拉伸，
        // 图层矩形会跟着变成 18×25 的椭圆（实测就是这个尺寸），而图像是居中等比画的，
        // 圆就一直是圆。
        b.image = Self.updateBadgeImage(18)
        b.widthAnchor.constraint(equalToConstant: 18).isActive = true
        b.heightAnchor.constraint(equalToConstant: 18).isActive = true
        b.isHidden = true
        return b
    }

    /// 蓝色实心圆 + 白色下载箭头（箭头手画，不依赖 SF Symbols 的着色行为）。
    /// 用户给的样式就是 Codex 那个：实心蓝圆 + 向下的箭头 + 底下一条托盘线。
    static func updateBadgeImage(_ d: CGFloat) -> NSImage {
        let img = NSImage(size: NSSize(width: d, height: d))
        img.lockFocus()
        NSColor.systemBlue.setFill()
        NSBezierPath(ovalIn: NSRect(x: 0, y: 0, width: d, height: d)).fill()
        let s = d / 18                      // 以 18 pt 为基准等比
        let p = NSBezierPath()
        p.lineWidth = 1.7 * s
        p.lineCapStyle = .round
        p.lineJoinStyle = .round
        let cx = d / 2
        p.move(to: NSPoint(x: cx, y: 11.6 * s))            // 竖杆顶端
        p.line(to: NSPoint(x: cx, y: 5.6 * s))             // 竖杆底端（箭头尖）
        p.move(to: NSPoint(x: cx - 2.8 * s, y: 8.5 * s))   // 左半边箭头
        p.line(to: NSPoint(x: cx, y: 5.6 * s))
        p.line(to: NSPoint(x: cx + 2.8 * s, y: 8.5 * s))   // 右半边箭头
        p.move(to: NSPoint(x: 5.6 * s, y: 3.3 * s))        // 托盘线
        p.line(to: NSPoint(x: 12.4 * s, y: 3.3 * s))
        NSColor.white.setStroke()
        p.stroke()
        img.unlockFocus()
        img.isTemplate = false               // 已经是最终颜色，别再被 tint 覆盖
        return img
    }

    /// 把两个更新标记同步到当前状态：有没有新版本、是不是在下载中、现在是哪种模式。
    /// 菜单栏图标上的橙点（`menuBarImage`）与菜单里那一条是另外两处，一起由
    /// `refreshUpdateUI()` 触发。
    func syncUpdateBadge() {
        let info = updateInfo
        let busy = !updatePhase.isEmpty
        let tip = info.map { busy ? updatePhase
                                  : L("有新版本 \($0.version) · 点击更新",
                                      "Version \($0.version) available · click to update") }
        for (badge, visible) in [(hudUpdateBadge, info != nil && !hudCompact),
                                 (hudUpdateBadgeCompact, info != nil && hudCompact)] {
            guard let b = badge else { continue }
            b.isHidden = !visible
            // 下载中不隐藏，只是压暗一点：把"正在装"告诉盯着挂件看的人，
            // 详细进度在菜单里那一条（每 5% 更新一次）
            b.alphaValue = busy ? 0.5 : 1
            b.toolTip = tip
            b.image = Self.updateBadgeImage(18)   // 换主题后重取 systemBlue
        }
    }

    @objc func hudClicked() {
        if hudCompact { toggleCompact() }   // collapsed: click expands
        else { openDashboard() }
    }

    /// Compact mode shows ONLY the rolling number; the panel hugs its content.
    @objc func toggleCompact() {
        hudCompact.toggle()
        for v in hudCollapsible { v.isHidden = hudCompact }
        if hudCompact {
            hudEst?.isHidden = true
            hudStatus?.isHidden = true
        } else {
            hudEst?.isHidden = hudEst?.stringValue.isEmpty ?? true
            hudRank?.isHidden = hudRank?.stringValue.isEmpty ?? true
        }
        setSymbol(hudCollapseBtn, hudCompact ? "chevron.right" : "chevron.down",
                  fallback: hudCompact ? "▸" : "▾")
        hudExpandBtn?.isHidden = !hudCompact
        // 两个更新标记分居两行（标题行 / 数字行），切模式时换一个显示
        syncUpdateBadge()
        resizeHUDToFit(animated: true)
    }

    /// Grow/shrink the panel to hug its visible content, keeping the top edge
    /// pinned. Non-animated calls come from live updates (rows appearing or
    /// disappearing) so the panel never ends up too short for its content.
    ///
    /// Height is free to grow and shrink; width only ever *gives back* space.
    /// That asymmetry is the fix for the ratchet described in STATUS.md 56:
    /// AppKit grows a window to satisfy its content's minimum width and never
    /// shrinks it again, so a single wide value widens the panel for the rest of
    /// the session. The number view has its own ceiling now, and this hands back
    /// a width that some *earlier* value already took — with hysteresis, so the
    /// one or two points by which AppKit's own minimum differs from the stack's
    /// fitting size can never make the panel oscillate.
    private func resizeHUDToFit(animated: Bool) {
        // a drag owns the frame: resizing mid-drag would both fight the pointer
        // and move the origin, which this function offsets by the height change.
        // The next poll after mouse-up picks the size change up instead.
        guard let panel = hudPanel, !hudDocked, !hudDragging else { return }
        let targetH = hudTargetHeight()
        // 宽度按内容算，**能长也能缩**：完整数字变长，面板就跟着变宽
        // （用户："如果数字长了，窗口宽度就自适应"）；数字缩回去，面板也缩回去。
        // 第 56 节那个"面板被撑到 531 pt 再也回不去"的棘轮，是因为当时靠 AppKit
        // 按内容的最小尺寸自己长，而没人把它收回来——现在每次都显式设一遍。
        // 收起态（只留数字）没有 292 pt 的下限，整个面板就是一个数字条。
        var targetW = panel.frame.width
        let needed = ceil((hudStack?.fittingSize.width ?? 0) + 32)
        let design = max(hudCompact ? 0 : 292, needed)
        if abs(targetW - design) > 1 { targetW = design }
        guard abs(panel.frame.height - targetH) > 1 || abs(panel.frame.width - targetW) > 1 else { return }
        var f = panel.frame
        f.origin.y += f.height - targetH
        f.size = NSSize(width: targetW, height: targetH)
        if animated {
            hudResizing = true
            // NSWindow's own animated setFrame is reliable; the animator()
            // proxy silently drops frames on borderless non-activating panels
            panel.setFrame(f, display: true, animate: true)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.35) { [weak self] in
                self?.hudResizing = false
            }
        } else {
            panel.setFrame(f, display: true)
        }
    }

    /// Dock the HUD to the right screen edge as a small bubble (today's total
    /// inside a spinning heat ring), or restore the full panel.
    @objc func toggleDock() {
        guard let panel = hudPanel else { return }
        hudDocked.toggle()
        if hudDocked {
            hudRestoreFrame = panel.frame
            // the full panel's Auto Layout constraints impose a ~300x199
            // minimum on the window — pull it out of the hierarchy so the
            // panel can actually shrink to the bubble size
            hudEffect?.removeFromSuperview()
            hudSide?.isHidden = false
            hudSide?.resetClock()
            startSpin()
            let size: CGFloat = 76
            let screen = NSScreen.screens.first(where: { $0.frame.width > 500 })
            // the bubble is *at* the right edge, so it is the element most likely
            // to end up under a right-side Dock: same visibleFrame rule
            let f = screen?.visibleFrame ?? NSRect(x: 0, y: 0, width: 1512, height: 982)
            let target = NSRect(x: f.maxX - size - 8,
                                y: f.midY - size / 2,
                                width: size, height: size)
            hudResizing = true
            panel.setFrame(target, display: true, animate: true)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { [weak self] in
                guard let self, let panel = self.hudPanel, let content = panel.contentView else { return }
                self.hudResizing = false
                // effect is out of the tree here; just keep the bubble square
                self.hudSide?.frame = content.bounds
            }
        } else {
            stopSpin()
            hudSide?.isHidden = true
            if let content = panel.contentView {
                if let effect = hudEffect, effect.superview == nil {
                    content.addSubview(effect)
                    if let side = hudSide { content.addSubview(side) }   // keep bubble on top
                }
                // the effect view still carries the 76x76 bubble frame from the
                // docked state — without this the panel restores to full size
                // while its content stays squeezed into the top corner
                hudEffect?.frame = content.bounds
                hudSide?.frame = content.bounds
            }
            var f = panel.frame
            f.size = NSSize(width: hudRestoreFrame.width, height: hudTargetHeight())
            if hudUserMoved {
                // come back to where the user had it, top edge preserved (the
                // panel hangs from its top edge when it resizes)
                f.origin = DragThroughView.clampToVisibleUnion(
                    NSPoint(x: hudRestoreFrame.minX, y: hudRestoreFrame.maxY - f.height),
                    size: f.size)
            } else if let screen = NSScreen.screens.first(where: { $0.frame.width > 500 }) {
                f.origin = NSPoint(x: screen.visibleFrame.maxX - f.width - 16,
                                   y: screen.visibleFrame.maxY - f.height - 8)
            }
            hudResizing = true
            panel.setFrame(f, display: true, animate: true)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { [weak self] in
                guard let self, let panel = self.hudPanel, let content = panel.contentView else { return }
                self.hudResizing = false
                content.layoutSubtreeIfNeeded()
                self.hudEffect?.frame = content.bounds
                self.hudSide?.frame = content.bounds
                self.resizeHUDToFit(animated: false)
                self.positionHUD()
            }
        }
        statusItem.menu = buildMenu()
    }

    /// A repeating timer in `.common` mode. The HUD's own animation must not
    /// stop just because a drag or a menu is running the run loop in another
    /// mode — a default-mode timer is simply suspended for the duration.
    @discardableResult
    private func addTimer(_ interval: TimeInterval, _ body: @escaping () -> Void) -> Timer {
        let t = Timer(timeInterval: interval, repeats: true) { _ in body() }
        RunLoop.main.add(t, forMode: .common)
        return t
    }

    // MARK: - Frame-level easing for the stretchy bar

    /// Ease `dispHeat` towards the target and put the result on the bar's layer
    /// transform. Runs at 60 Hz, and only while the value is still moving —
    /// an idle HUD should cost nothing, which is the same rule the ring follows
    /// ("无 agent 工作时光环不再空转").
    private func smoothStep() {
        let now = CACurrentMediaTime()
        // Clamp dt: the first step after the timer starts, or after the machine
        // wakes, must not make the bar jump.
        let dt = min(max(now - lastSmoothAt, 1.0 / 240.0), 0.25)
        lastSmoothAt = now
        // Same time constant as the old 12.5 Hz step (`+= diff * 0.10` every
        // 0.08 s), expressed so it stays correct at any frame rate.
        let k = 1 - pow(1 - 0.10, dt / 0.08)
        dispHeat += (lastHeat - dispHeat) * k
        if abs(lastHeat - dispHeat) < 0.002 { dispHeat = lastHeat }
        let ratio = CGFloat(min(max(dispHeat, 0), 1))
        if abs(ratio - shownBarRatio) > 0.001 {
            shownBarRatio = ratio
            // CATransform3D on the layer: no layout pass, no constraint, just a
            // GPU transform of a bar that is anchored at its left edge.
            hudBarFill?.layer?.transform = CATransform3DMakeScale(max(ratio, 0.0001), 1, 1)
        }
        if dispHeat == lastHeat { stopSmooth() }
    }

    private func startSmooth() {
        if smoothTimer != nil { return }
        lastSmoothAt = CACurrentMediaTime()
        let t = Timer(timeInterval: 1.0 / 60.0, repeats: true) { [weak self] _ in
            self?.smoothStep()
        }
        // A little tolerance lets the system coalesce wake-ups (it is a visual
        // nicety, not a clock), which keeps the idle-to-active cost low.
        t.tolerance = 0.004
        RunLoop.main.add(t, forMode: .common)   // keep moving during drags/menus
        smoothTimer = t
    }

    private func stopSmooth() {
        smoothTimer?.invalidate()
        smoothTimer = nil
    }

    private func startSpin() {
        spinTimer?.invalidate()
        let t = Timer(timeInterval: 1.0 / 30.0, repeats: true) { [weak self] _ in
            self?.hudSide?.advance()
        }
        // .common keeps the ring moving during window drags and menu tracking
        RunLoop.main.add(t, forMode: .common)
        spinTimer = t
    }

    private func stopSpin() {
        spinTimer?.invalidate()
        spinTimer = nil
    }

    @objc func toggleHUD() {
        if hudVisible {
            hudPanel?.orderOut(nil); hudVisible = false
            stopSpin()                        // no point spinning an invisible ring
        } else {
            showHUD()
            if hudDocked { startSpin() }
        }
        statusItem.menu = buildMenu()
    }

    // MARK: - Dashboard window

    @objc func openUpdateURL() {
        guard let u = updateInfo else { return }
        // The backend answers with a path (`/download`) because it does not know
        // which hostname it is reached by — a tunnel, a domain, an IP. Resolve it
        // against the same origin we just asked, so the update link follows the
        // user's own `tb_tel_url`.
        guard let url = resolve(u.url) else { return }
        NSWorkspace.shared.open(url)
    }

    @objc func openDashboard() {
        showDashboard(path: "/")
    }

    /// Opens (or reuses) the dashboard window, optionally on a sub-page.
    func showDashboard(path: String) {
        let url = URL(string: "http://127.0.0.1:8737" + path)!
        if let w = dashboardWindow { w.makeKeyAndOrderFront(nil); NSApp.activate(ignoringOtherApps: true); return }
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1280, height: 860),
                         styleMask: [.titled, .closable, .miniaturizable, .resizable],
                         backing: .buffered, defer: false)
        // NSWindow over-releases *programmatically created* windows when they
        // are closed (isReleasedWhenClosed defaults to true for them), which
        // drops the retain count below what ARC owns and leaves
        // `dashboardWindow` pointing at freed memory. The next call then
        // segfaults inside -[NSWindow makeKeyAndOrderFront:]. ARC keeps this
        // window alive through `dashboardWindow`, so AppKit must not release it.
        w.isReleasedWhenClosed = false
        w.title = "TokenDance"
        w.center()
        let webView = WKWebView(frame: w.contentView?.bounds ?? w.frame)
        webView.autoresizingMask = [.width, .height]
        webView.uiDelegate = self
        w.contentView?.addSubview(webView)
        webView.load(URLRequest(url: url))
        dashboardWeb = webView
        dashboardWindow = w
        w.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    /// App menu → About. The about page ships inside the app, so this works
    /// offline and in the same window the dashboard uses.
    @objc func showAbout() {
        if let web = dashboardWeb, dashboardWindow != nil {
            web.load(URLRequest(url: URL(string: "http://127.0.0.1:8737/about")!))
            dashboardWindow?.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
        } else {
            showDashboard(path: "/about")
        }
    }

    @objc func refreshNow() {
        refresh()
        pollLive()
        lbTick()
        pingTelemetry()
    }
}

/// Entry point.
///
/// This used to be five bare statements at the bottom of the file, which only
/// ever compiled because `swiftc` was handed a single source: with more than one
/// input file, only a file literally named `main.swift` may contain top-level
/// code, and every other file is compiled in library mode. Adding
/// `RingRenderer.swift` therefore broke the build outright. An explicit `@main`
/// type works from any file and no longer depends on how many sources exist.
@main
enum TokenDanceApp {
    /// `NSApplication.delegate` is a weak reference, and the HUD installs
    /// closures that need a way back to the delegate. Capturing `self` in those
    /// closures would close a cycle (delegate → panel → content → closure →
    /// delegate), so the process keeps one strong handle here instead.
    static var delegate: AppDelegate!

    static func main() {
        let app = NSApplication.shared
        delegate = AppDelegate()
        app.delegate = delegate
        // .regular, not .accessory: this is what puts the app in the Dock (and
        // in ⌘Tab, with a menu bar of its own). The floating HUD is a
        // non-activating panel, so it still does not steal focus when it
        // appears — the two are independent.
        app.setActivationPolicy(.regular)
        app.run()
    }
}
