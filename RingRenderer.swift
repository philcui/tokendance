// Comet ring renderer for the edge-docked HUD bubble.
//
// The old ring was a constant-width 110° stroke with a glowing dot at the head.
// A constant-width arc with two abrupt ends reads as a "rotating pie slice" —
// the eye weights both ends equally and there is no sense of a swept trail. What
// the eye wants is a comet: one bright head, and behind it a trail that both
// narrows to a point and fades out, sitting inside a real bloom.
//
// Three mechanisms, each doing one job:
//
//   * Width taper  — the clip path is a ribbon whose thickness follows
//                    `halfW * t^taperPower`, thin at the tail and swelling to
//                    full width at the head. An exponent > 1 is what makes the
//                    tail *pointed* rather than a blunt wedge (the same idea as
//                    the negative "ease" on After Effects' tapered strokes).
//                    This lives in the path, so it works on every OS version.
//   * Opacity tail — a conic gradient supplies the along-the-arc fade. Because
//                    the fill is one continuous gradient clipped to the ribbon,
//                    there is no banding: filling the ribbon as a chain of
//                    translucent quads (the obvious approach) double-blends at
//                    every seam and shows as stripes.
//   * Bloom        — the whole ribbon is composited once inside a transparency
//                    layer and blurred as a group, so Core Graphics rasterises
//                    the blur once rather than once per element. A second,
//                    slightly wider ribbon at low alpha sits underneath to give
//                    the light body; the head adds two feathered radial passes
//                    (a wide one for spread, a tight one for the hot core), and
//                    in dark mode the head passes are composited with
//                    `.plusLighter` so they *add* light instead of painting a
//                    grey halo over it.
//
// Availability note: `CGContext.drawConicGradient` exists in the Swift overlay
// only from macOS 14, and the underlying `CGContextDrawConicGradient` is
// annotated `CG_AVAILABLE_STARTING(14.0, 17.0)` — not 13.0 as is often assumed.
// Call the C symbol directly behind a 14.0 guard. On anything older the width
// taper and the round cap still apply (both live in the path) and the ribbon is
// filled flat, which is a uniform-alpha pointed wedge — deliberately *not* a
// chain of short arcs, which double-blends at every seam and shows as stripes.

import AppKit

enum RingRenderer {

    struct Style {
        /// Ribbon thickness at the head. The tail tapers to a point.
        var headWidth: CGFloat = 4.2
        /// How far the trail reaches around the circle, in radians. The view
        /// drives this from the smoothed burn rate so the trail visibly
        /// stretches when the agent works harder.
        var sweep: CGFloat = 260 * .pi / 180
        /// Width profile exponent (1 = blunt linear wedge, higher = sharper tail).
        var taperPower: CGFloat = 2.3
        /// Opacity profile exponent along the trail.
        var fadePower: CGFloat = 1.7
        /// Alpha the tail tip fades to — a floor keeps the wisp visible instead
        /// of letting the last third of the trail vanish into nothing.
        var tailAlpha: CGFloat = 0.10
        /// How far the head colour is pushed towards white (0 = plain heat colour).
        var coreLighten: CGFloat = 0.62
        /// Alpha of the surrounding bloom.
        var haloAlpha: CGFloat = 0.50
        /// Blur radius of the bloom, in points.
        var haloBlur: CGFloat = 6.5
        /// Extra under-glow ribbon width, in multiples of headWidth. Keep this
        /// close to 1: the under-glow cannot be feathered, so every point of
        /// extra width shows as a second hard edge running parallel to the
        /// filament. It is there to give the light body, not to be a second
        /// ribbon.
        var glowSpread: CGFloat = 1.75
        /// Under-glow alpha, relative to the bloom.
        var glowAlpha: CGFloat = 0.22
        /// Head bloom radius, in multiples of headWidth.
        var headBloomScale: CGFloat = 1.9
        /// Alpha of the white-hot centre of the head bloom.
        var headCoreAlpha: CGFloat = 0.55
        /// Dim full ring behind the comet.
        var trackAlpha: CGFloat = 0.10
        var trackWidth: CGFloat = 3.5

        /// Light glass is frosted white, so a white-hot core would disappear —
        /// there the saturated halo carries the glow and the core keeps its own
        /// colour. Dark glass is the reverse: the core is the light source.
        /// Additive blending is likewise a dark-mode-only luxury: on a white
        /// disc, `plusLighter` would just blow the halo out to a grey smear.
        static func forTheme(dark: Bool) -> Style {
            var s = Style()
            if !dark {
                s.coreLighten = 0.30
                s.haloAlpha = 0.30
                s.haloBlur = 5.5
                s.glowAlpha = 0.12
                s.headBloomScale = 1.7
                s.headCoreAlpha = 0.30
                s.trackAlpha = 0.13
            }
            return s
        }
    }

    // MARK: - colour helpers

    /// `NSColor.blended(withFraction:of:)` is only defined between colours in the
    /// same colour space and blows up otherwise, so pin both ends to deviceRGB
    /// first. Dynamic colours resolve against `NSAppearance.current`, which
    /// `draw(_:)` has already set to the view's appearance.
    private static func mix(_ a: NSColor, _ b: NSColor, _ f: CGFloat) -> NSColor {
        let x = a.usingColorSpace(.deviceRGB) ?? a
        let y = b.usingColorSpace(.deviceRGB) ?? b
        let t = min(max(f, 0), 1)
        return x.blended(withFraction: t, of: y) ?? x
    }

    private static func rgb(_ c: NSColor) -> NSColor {
        c.usingColorSpace(.deviceRGB) ?? c
    }

    // MARK: - geometry

    /// The tapered ribbon, as a clip path. Cross-sections are radial, which is
    /// what a ring band wants. Generated as one closed subpath (band forward,
    /// round cap, band back) so no even-odd fill rule is needed.
    ///
    /// The cap matters more than it looks. A band that just stops is a flat
    /// radial cut, and once the bloom is layered on top the cut reads as a bead
    /// impaled on a wire. Giving the head end a real semicircular cap of radius
    /// `halfW` — extending `atan(halfW/radius)` past `tailAngle + sweep` — makes
    /// the head terminate the way a comet does. The returned angle is that
    /// overhang, which the gradient needs so it stays lit across the whole cap.
    ///
    /// `headTrim` shrinks the band to nothing over the last fraction of the
    /// sweep. The under-glow uses it: a widened ribbon that is *also* capped
    /// sticks out past the filament's cap by half the width difference and shows
    /// as a second, hard-edged disc around the head. Fading it out first leaves
    /// the head glow to the shadow bloom and the radial hotspot, which are soft
    /// by construction.
    private static func ribbonPath(center: CGPoint, radius: CGFloat,
                                   tailAngle: CGFloat, sweep: CGFloat,
                                   headWidth: CGFloat, taperPower: CGFloat,
                                   headTrim: CGFloat = 0,
                                   steps: Int = 72) -> (path: CGPath, capAngle: CGFloat) {
        let halfW = headWidth / 2
        let headAngle = tailAngle + sweep
        let p = CGMutablePath()
        func point(_ t: CGFloat, _ side: CGFloat) -> CGPoint {
            let a = tailAngle + sweep * t
            var w = pow(max(t, 0.0001), taperPower)
            if headTrim > 0 {
                // smootherstep window that reaches zero exactly at t = 1
                let e = min(1, max(0, (1 - t) / headTrim))
                w *= e * e * (3 - 2 * e)
            }
            let r = radius + side * halfW * w
            return CGPoint(x: center.x + r * cos(a), y: center.y + r * sin(a))
        }
        for i in 0...steps {
            let q = point(CGFloat(i) / CGFloat(steps), 1)
            if i == 0 { p.move(to: q) } else { p.addLine(to: q) }
        }
        // round cap, built from points in the local (radial, tangential) frame:
        // phi = 0 is the outer band edge, 90 the point straight ahead, 180 the
        // inner band edge — so the arc bulges along the direction of travel and
        // its two ends coincide exactly with the band edges. (Starting phi at
        // 90 instead of 0 rotates the whole cap a quarter turn and leaves a
        // chord-shaped gap between cap and band.)
        let capCenter = CGPoint(x: center.x + radius * cos(headAngle),
                                y: center.y + radius * sin(headAngle))
        let ux = cos(headAngle), uy = sin(headAngle)
        let vx = -sin(headAngle), vy = cos(headAngle)
        let capSteps = 14
        for i in 1...capSteps {
            let phi = (180 * CGFloat(i) / CGFloat(capSteps)) * .pi / 180
            p.addLine(to: CGPoint(x: capCenter.x + halfW * (cos(phi) * ux + sin(phi) * vx),
                                  y: capCenter.y + halfW * (cos(phi) * uy + sin(phi) * vy)))
        }
        for i in stride(from: steps, through: 0, by: -1) {
            p.addLine(to: point(CGFloat(i) / CGFloat(steps), -1))
        }
        p.closeSubpath()
        // Cap overhang, measured from the band's *actual* width at the head —
        // with `headTrim` the band converges to a point there, and reporting
        // `atan(halfW/radius)` anyway would leave the gradient lit across an
        // arc where there is no ribbon left to light.
        let headHalf = headTrim > 0 ? 0 : halfW
        return (p, headHalf > 0 ? atan(headHalf / radius) : 0)
    }

    /// Conic gradient that fades in from the tail tip, is fully lit at the head,
    /// and is transparent for the rest of the circle. `angle` origin for a conic
    /// gradient is +x with increasing angle towards +y, which matches the
    /// `cos/sin` used above, so the same `tailAngle` is handed to both.
    ///
    /// `extent` is the lit span — band sweep *plus* the head cap overhang — so
    /// the cap is lit all the way across instead of being sliced in half.
    private static func trailGradient(color: NSColor, hot: NSColor,
                                      extent: CGFloat, fadePower: CGFloat,
                                      tailAlpha: CGFloat) -> CGGradient? {
        let span = min(1, extent / (2 * .pi))
        let steps = 18
        var colors: [CGColor] = []
        var locations: [CGFloat] = []
        // i < steps, not <= steps: the loop already lands on `span` at its last
        // sample, and re-appending `span` below would give CGGradient two stops
        // at one location. Duplicate locations are not a defined interpolation —
        // it invites a seam exactly at the head, where it is most visible.
        for i in 0..<steps {
            let u = CGFloat(i) / CGFloat(steps)
            let a = tailAlpha + (1 - tailAlpha) * pow(u, fadePower)
            // Head end warms towards the hot colour so the leading edge reads as
            // the source of the light rather than a flat cut.
            let c = mix(color, hot, pow(u, 2.6) * 0.8)
            colors.append(rgb(c).withAlphaComponent(a).cgColor)
            locations.append(u * span)
        }
        // full brightness all the way to the tip of the cap, then cut off
        colors.append(rgb(hot).withAlphaComponent(1).cgColor)
        locations.append(span)
        colors.append(rgb(color).withAlphaComponent(0).cgColor)
        locations.append(min(1, span + 0.0005))
        colors.append(rgb(color).withAlphaComponent(0).cgColor)
        locations.append(1)
        return CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(),
                          colors: colors as CFArray, locations: locations)
    }

    // MARK: - draw

    static func draw(in ctx: CGContext, center: CGPoint, radius: CGFloat,
                     headAngle: CGFloat, color: NSColor, style: Style,
                     dark: Bool = true, drawTrack: Bool = true) {
        if drawTrack {
            rgb(dark ? NSColor.white : NSColor.black)
                .withAlphaComponent(style.trackAlpha).setStroke()
            let track = NSBezierPath()
            track.appendArc(withCenter: center, radius: radius, startAngle: 0, endAngle: 360)
            track.lineWidth = style.trackWidth
            track.stroke()
        }

        let sweep = style.sweep
        let tailAngle = headAngle - sweep
        let hotspot = mix(color, .white, style.coreLighten)

        // under-glow: a widened copy of the same ribbon, faded out before the
        // head so the light has body around the filament without becoming a
        // second disc. Normal blending on purpose — this is meant to add
        // saturated colour, not add light, or the head region blows out to
        // cream and the "hot = red" reading is lost.
        if style.glowAlpha > 0 {
            let glowW = style.headWidth * style.glowSpread
            let (glow, glowCap) = ribbonPath(center: center, radius: radius,
                                             tailAngle: tailAngle, sweep: sweep,
                                             headWidth: glowW,
                                             taperPower: style.taperPower * 0.85,
                                             headTrim: 0.18, steps: 48)
            ctx.saveGState()
            ctx.addPath(glow)
            ctx.clip()
            if #available(macOS 14.0, *),
               let g = trailGradient(color: color, hot: hotspot,
                                     extent: sweep + glowCap,
                                     fadePower: style.fadePower * 0.75,
                                     tailAlpha: style.tailAlpha * 0.5) {
                CGContextDrawConicGradient(ctx, g, center, tailAngle)
            } else {
                rgb(color).withAlphaComponent(style.glowAlpha).setFill()
                ctx.fill(CGRect(x: center.x - radius - glowW, y: center.y - radius - glowW,
                                width: (radius + glowW) * 2, height: (radius + glowW) * 2))
            }
            ctx.restoreGState()
        }

        let (clip, capAngle) = ribbonPath(center: center, radius: radius,
                                          tailAngle: tailAngle, sweep: sweep,
                                          headWidth: style.headWidth,
                                          taperPower: style.taperPower)

        if #available(macOS 14.0, *),
           let grad = trailGradient(color: color, hot: hotspot,
                                    extent: sweep + capAngle,
                                    fadePower: style.fadePower,
                                    tailAlpha: style.tailAlpha) {
            // 1) bloom: the ribbon is composited once and blurred as a group, so
            //    the shadow costs one rasterisation rather than one per element
            ctx.saveGState()
            ctx.setShadow(offset: .zero, blur: style.haloBlur,
                          color: rgb(color).withAlphaComponent(style.haloAlpha).cgColor)
            ctx.beginTransparencyLayer(auxiliaryInfo: nil)
            ctx.saveGState()
            ctx.addPath(clip)
            ctx.clip()
            CGContextDrawConicGradient(ctx, grad, center, tailAngle)
            ctx.restoreGState()
            ctx.endTransparencyLayer()
            ctx.restoreGState()

            // 2) crisp core on top of its own glow
            ctx.saveGState()
            ctx.addPath(clip)
            ctx.clip()
            CGContextDrawConicGradient(ctx, grad, center, tailAngle)
            ctx.restoreGState()
        } else {
            // Pre-macOS-14 has no conic gradient. The width taper and the round
            // cap still live in the path, so fill the ribbon flat — a
            // uniform-alpha pointed wedge beats a chain of short arcs, which
            // double-blends at every seam and shows as stripes.
            ctx.saveGState()
            if dark { ctx.setBlendMode(.plusLighter) }
            ctx.addPath(clip)
            ctx.clip()
            rgb(color).withAlphaComponent(0.72 * (1 - style.tailAlpha) + style.tailAlpha).setFill()
            ctx.fill(CGRect(x: center.x - radius - style.headWidth,
                            y: center.y - radius - style.headWidth,
                            width: (radius + style.headWidth) * 2,
                            height: (radius + style.headWidth) * 2))
            ctx.restoreGState()
        }

        // 3) head: two soft radial passes on the cap centre — a wide one that
        //    gives the bloom its spread, and a tight one for the hot core.
        //    An earlier version drew the core as a flat `fillEllipse`; because
        //    the surrounding bloom is composited additively it had already been
        //    pushed to near-white, so a normal-blended disc at 0.92 alpha landed
        //    *darker* than its own halo and read as a hole punched through the
        //    light. A feathered gradient cannot do that at any size.
        let head = CGPoint(x: center.x + radius * cos(headAngle),
                           y: center.y + radius * sin(headAngle))
        let space = CGColorSpaceCreateDeviceRGB()
        let bloomR = max(style.headWidth * style.headBloomScale, 4)
        if let bloom = CGGradient(colorsSpace: space,
                                  colors: [rgb(hotspot).withAlphaComponent(style.headCoreAlpha).cgColor,
                                           rgb(color).withAlphaComponent(style.headCoreAlpha * 0.55).cgColor,
                                           rgb(color).withAlphaComponent(0).cgColor] as CFArray,
                                  locations: [0, 0.38, 1]) {
            ctx.saveGState()
            if dark { ctx.setBlendMode(.plusLighter) }
            ctx.drawRadialGradient(bloom, startCenter: head, startRadius: 0,
                                   endCenter: head, endRadius: bloomR, options: [])
            ctx.restoreGState()
        }
        let coreR = max(style.headWidth * 0.78, 2)
        if let core = CGGradient(colorsSpace: space,
                                 colors: [rgb(hotspot).withAlphaComponent(0.9).cgColor,
                                          rgb(hotspot).withAlphaComponent(0.35).cgColor,
                                          rgb(hotspot).withAlphaComponent(0).cgColor] as CFArray,
                                 locations: [0, 0.45, 1]) {
            ctx.saveGState()
            if dark { ctx.setBlendMode(.plusLighter) }
            ctx.drawRadialGradient(core, startCenter: head, startRadius: 0,
                                   endCenter: head, endRadius: coreR, options: [])
            ctx.restoreGState()
        }
    }
}
