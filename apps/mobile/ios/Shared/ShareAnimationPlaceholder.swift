import SwiftUI
import UIKit

/// Share card colours (mockup theme.css `--share-*`); the tail and the bar follow the accent.
private struct ShareColors {
    let surface, border, track, text, device, arrow, scrim, tail, bar, offline: Color
    init(_ p: OinkPalette, failed: Bool = false) {
        surface = p.shareSurface; border = p.shareBorder; track = p.shareTrack; text = p.shareText
        device = p.shareText; arrow = p.shareArrow; scrim = p.shareScrim; tail = p.accent; bar = failed ? p.bad : p.accentInk
        offline = p.faint
    }
}

/// The share card's small capsule buttons (取消 / 关闭): 30 high, chip fill (p2p.css `.sh-cancel`);
/// 16 pt above them, 4 more than before (review 2026-11-11).
struct ShareCapsuleButtonStyle: ButtonStyle {
    @Environment(\.oink) private var oink
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 13, weight: .medium)).foregroundStyle(oink.text2)
            .padding(.horizontal, 14).frame(height: 30)
            .background(oink.chip, in: Capsule())
            .opacity(configuration.isPressed ? 0.7 : 1)
            .padding(.top, 16)
    }
}

/// What one stage frame shows: the flight (nil: no arrow) and the waiting marks' opacities.
private struct StageFrame {
    var flight: ShareAnimFrame?
    var arrowAlpha: CGFloat = 1
    var logo: CGFloat = 0
    var offline: CGFloat = 0
    static let waiting = StageFrame(flight: nil, logo: 1, offline: 1)
}

/// The 240 × 88 stage: this phone → flying arrow with its accent tail → computer, or phone
/// (`state.toPhone`, SPEC §3.6). While the share waits for its offline target the stage stands
/// still with the Oink mark on this phone and the offline mark on the target (§2.1); `onlineAt`
/// runs the 300 ms hand-over to sending, `waitAt` the 150 ms entry into the wait (§5.9).
struct ShareFlightView: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.oink) private var oink
    let state: ShareUiState
    /// Fixed animation phase / hand-over moment (design-state screenshots).
    var freezeT: CGFloat?
    var freezeHandoff: CGFloat? = nil
    var onlineAt: Date? = nil
    var waitAt: Date? = nil
    private var target: ShareTarget { state.toPhone ? .phone : .computer }
    var body: some View {
        let moving = state.phase == .preparing || state.phase == .sending
        Group {
            if state.phase == .succeeded { canvas(StageFrame(flight: nil), landed: true) }
            // Failed: the two devices without the arrow (mockup ShareCard.jsx).
            else if state.phase == .failed { canvas(StageFrame(flight: nil)) }
            else if let freezeHandoff { canvas(handoffFrame(ShareAnim.handoff(freezeHandoff))) }
            else if state.phase == .waiting {
                if let waitAt, !reduceMotion {
                    TimelineView(.animation) { clock in
                        let x = ShareAnim.enterWait(CGFloat(clock.date.timeIntervalSince(waitAt) * 1000))
                        canvas(StageFrame(flight: ShareAnim.frame(loopT(clock.date), target), arrowAlpha: 1 - x, logo: x, offline: x))
                    }
                } else { canvas(.waiting) }
            }
            else if let freezeT { canvas(StageFrame(flight: ShareAnim.frame(freezeT, target))) }
            else if reduceMotion || !moving { canvas(StageFrame(flight: ShareAnim.frame(ShareAnim.staticT, target))) }
            else {
                TimelineView(.animation) { clock in
                    if let onlineAt {
                        let ms = CGFloat(clock.date.timeIntervalSince(onlineAt) * 1000)
                        if ms < ShareAnim.handoffMs { canvas(handoffFrame(ShareAnim.handoff(ms))) }
                        // After the hand-over the rounds count from 160 ms after it began.
                        else { canvas(StageFrame(flight: ShareAnim.frame(((ms - ShareAnim.leadMs).truncatingRemainder(dividingBy: ShareAnim.duration)) / ShareAnim.duration, target))) }
                    } else {
                        canvas(StageFrame(flight: ShareAnim.frame(loopT(clock.date), target)))
                    }
                }
            }
        }
        .frame(width: 240, height: 88)
        .accessibilityHidden(true)
    }
    private func loopT(_ date: Date) -> CGFloat {
        CGFloat((date.timeIntervalSinceReferenceDate * 1000).truncatingRemainder(dividingBy: Double(ShareAnim.duration))) / ShareAnim.duration
    }
    private func handoffFrame(_ h: ShareHandoff) -> StageFrame {
        StageFrame(flight: h.t.map { ShareAnim.frame($0, target) }, logo: h.logo, offline: h.offline)
    }
    private func canvas(_ f: StageFrame, landed: Bool = false) -> some View {
        Canvas { c, _ in draw(&c, f, landed: landed) }
    }

    private func draw(_ c: inout GraphicsContext, _ f: StageFrame, landed: Bool) {
        let colors = ShareColors(oink)
        let line = StrokeStyle(lineWidth: 2, lineCap: .butt, lineJoin: .round)
        c.stroke(ShareAnim.phone(), with: .color(colors.device), style: line)
        let nudge = landed ? 1 : f.flight?.nudge ?? 1
        let center = target.nudgeCenter
        c.drawLayer { l in
            l.translateBy(x: center.x, y: center.y); l.scaleBy(x: nudge, y: nudge); l.translateBy(x: -center.x, y: -center.y)
            l.stroke(state.toPhone ? ShareAnim.farPhone() : ShareAnim.computer(), with: .color(colors.device), style: line)
        }
        func mark(_ transform: CGAffineTransform, _ alpha: CGFloat) {
            c.drawLayer { l in
                l.opacity = Double(alpha)
                l.stroke(LandedMark.tail(transform), with: .color(colors.tail), style: StrokeStyle(lineWidth: 5.5 * transform.a, lineCap: .round))
                let arrow = LandedMark.arrow(transform)
                l.fill(arrow, with: .color(colors.arrow))
                l.stroke(arrow, with: .color(colors.arrow), style: StrokeStyle(lineWidth: 5 * transform.a, lineJoin: .round))
            }
        }
        if landed { mark(state.toPhone ? LandedMark.phone : LandedMark.computer, 1) }
        if f.logo > 0.001 { mark(LandedMark.local, f.logo) }
        if f.offline > 0.001 {
            let o = target.offlineOrigin
            c.drawLayer { l in
                // Arcs and dot, then the slash cut out of the arcs by a surface-coloured stroke (SPEC §3.8).
                l.opacity = Double(f.offline)
                l.translateBy(x: o.x, y: o.y)
                let round = StrokeStyle(lineWidth: 2, lineCap: .round)
                l.stroke(ShareAnim.offlineArcs(), with: .color(colors.offline), style: round)
                l.fill(Path(ellipseIn: CGRect(x: -1.6, y: -1.6, width: 3.2, height: 3.2)), with: .color(colors.offline))
                l.stroke(ShareAnim.offlineSlash(), with: .color(colors.surface), style: StrokeStyle(lineWidth: 4, lineCap: .round))
                l.stroke(ShareAnim.offlineSlash(), with: .color(colors.offline), style: round)
            }
        }
        guard let flight = f.flight else { return }
        let opacity = flight.opacity * f.arrowAlpha
        if opacity > 0.001, let t = ShareAnim.trail(flight.trail) {
            let g = Gradient(stops: [.init(color: colors.tail, location: 0), .init(color: colors.tail.opacity(0.8), location: 0.4),
                                     .init(color: colors.tail.opacity(0), location: 1)])
            c.opacity = Double(opacity)
            c.fill(t.0, with: .linearGradient(g, startPoint: t.1, endPoint: t.2))
            c.opacity = 1
        }
        if opacity > 0.001 {
            c.drawLayer { l in
                l.opacity = Double(opacity)
                l.translateBy(x: flight.x, y: flight.y); l.rotate(by: .radians(Double(flight.angle))); l.scaleBy(x: flight.scale, y: flight.scale)
                let p = ShareAnim.arrow()
                l.stroke(p, with: .color(colors.surface), style: StrokeStyle(lineWidth: 6, lineJoin: .round))
                l.fill(p, with: .color(colors.arrow))
                l.stroke(p, with: .color(colors.arrow), style: StrokeStyle(lineWidth: 3, lineJoin: .round))
            }
        }
    }
}

/// Endpoint-parameterised SVG arc ("A r r 0 large sweep x y") appended to a path.
private func addSVGArc(_ path: inout Path, from p1: CGPoint, to p2: CGPoint, radius r: CGFloat, large: Bool, sweep: Bool) {
    let dx = (p1.x - p2.x) / 2, dy = (p1.y - p2.y) / 2
    let d2 = dx * dx + dy * dy
    let rr = max(r, sqrt(d2))
    var k = sqrt(max(0, (rr * rr - d2) / d2))
    if large == sweep { k = -k }
    let cx = k * dy + (p1.x + p2.x) / 2, cy = -k * dx + (p1.y + p2.y) / 2
    let a1 = atan2(p1.y - cy, p1.x - cx), a2 = atan2(p2.y - cy, p2.x - cx)
    // In y-down coordinates SVG sweep 1 runs with increasing angle, which SwiftUI calls clockwise = false.
    path.addArc(center: CGPoint(x: cx, y: cy), radius: rr, startAngle: .radians(Double(a1)), endAngle: .radians(Double(a2)), clockwise: !sweep)
}

/// The Oink mark that lands on the computer screen (ShareCard.jsx `sh-landed`), in stage units.
private enum LandedMark {
    static let computer = CGAffineTransform(translationX: 187.3, y: 38.3).scaledBy(x: 0.4, y: 0.4).translatedBy(x: 0, y: 1.5)
    /// On the target phone's screen (SPEC §3.5): `translate(193.8 42.8) scale(0.32) translate(0 1.5)`.
    static let phone = CGAffineTransform(translationX: 193.8, y: 42.8).scaledBy(x: 0.32, y: 0.32).translatedBy(x: 0, y: 1.5)
    /// On this phone's screen while waiting (SPEC §3.7): the content is ready to go.
    static let local = CGAffineTransform(translationX: 25.8, y: 42.8).scaledBy(x: 0.32, y: 0.32).translatedBy(x: 0, y: 1.5)
    static func tail(_ transform: CGAffineTransform) -> Path {
        var p = Path()
        p.move(to: CGPoint(x: 20.5, y: 32))
        p.addCurve(to: CGPoint(x: 45, y: 54.5), control1: CGPoint(x: 20.5, y: 46), control2: CGPoint(x: 31, y: 54.5))
        addSVGArc(&p, from: CGPoint(x: 45, y: 54.5), to: CGPoint(x: 36.3, y: 41.2), radius: 9.5, large: true, sweep: false)
        return p.applying(transform)
    }
    static func arrow(_ transform: CGAffineTransform) -> Path {
        var p = Path()
        p.move(to: CGPoint(x: 9, y: 6)); p.addLine(to: CGPoint(x: 9, y: 39)); p.addLine(to: CGPoint(x: 33.5, y: 29.5))
        p.addQuadCurve(to: CGPoint(x: 9, y: 6), control: CGPoint(x: 23.5, y: 15.5)); p.closeSubpath()
        return p.applying(transform)
    }
}

/// 240 × 6 progress track; indeterminate = a 35 % segment sliding every 1400 ms
/// (reduced motion: the whole bar breathes).
struct ShareProgressBar: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.oink) private var oink
    let progress: Double?
    var freezeT: CGFloat?
    var failed = false
    /// The target came online (hand-over): the indeterminate segment shares the flight's t, which
    /// starts 160 ms later, so until then it waits left of the track (SPEC §5.9).
    var onlineAt: Date? = nil
    var body: some View {
        let colors = ShareColors(oink, failed: failed)
        GeometryReader { g in
            ZStack(alignment: .leading) {
                if let progress {
                    Capsule().fill(colors.bar).frame(width: max(0, g.size.width * min(1, max(0, progress))))
                } else if let freezeT {
                    segment(colors.bar, width: g.size.width, t: freezeT)
                } else if reduceMotion {
                    TimelineView(.animation(minimumInterval: 0.05)) { clock in
                        let pulse = sin(clock.date.timeIntervalSinceReferenceDate * 2 * .pi / 2.4)
                        Capsule().fill(colors.bar.opacity(0.525 + 0.175 * pulse))
                    }
                } else {
                    TimelineView(.animation) { clock in
                        if let onlineAt {
                            let ms = max(0, CGFloat(clock.date.timeIntervalSince(onlineAt) * 1000) - ShareAnim.leadMs)
                            segment(colors.bar, width: g.size.width, t: ms.truncatingRemainder(dividingBy: ShareAnim.duration) / ShareAnim.duration)
                        } else {
                            let ms = clock.date.timeIntervalSinceReferenceDate * 1000
                            segment(colors.bar, width: g.size.width, t: CGFloat(ms.truncatingRemainder(dividingBy: 1400) / 1400))
                        }
                    }
                }
            }
        }
        .frame(width: 240, height: 6)
        .background(colors.track, in: Capsule())
        .clipShape(Capsule())
        .accessibilityElement()
        .accessibilityLabel("发送进度")
        .accessibilityValue(progress.map { "\(Int($0 * 100))%" } ?? "进行中")
    }
    private func segment(_ color: Color, width: CGFloat, t: CGFloat) -> some View {
        let e = ShareAnim.easeStandard(t)
        return Capsule().fill(color).frame(width: width * 0.35).offset(x: width * (-0.35 + 1.35 * e))
    }
}

/// Share card (mockup `ShareCard.jsx`) over its scrim; in the share extension the scrim dims only
/// the extension's own picker page, which shows through it (user 2026-11-12).
/// Waiting for the offline target (share-anim SPEC §2.1): still stage, two lines, no progress bar,
/// only 取消. When the target comes online it hands over to sending in 300 ms; entering the wait
/// from sending takes 150 ms (§5.9). Reduced motion switches without transitions (§7).
struct ShareCardView<Content: View>: View {
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let state: ShareUiState
    var freezeT: CGFloat?
    /// A fixed hand-over moment (ms after the target came online) for design scenes.
    var freezeHandoff: CGFloat? = nil
    @ViewBuilder let content: () -> Content
    @State private var onlineAt: Date?
    @State private var waitAt: Date?
    private var waiting: Bool { state.phase == .waiting }
    var body: some View {
        let colors = ShareColors(oink)
        let still = reduceMotion || freezeT != nil || freezeHandoff != nil
        ZStack {
            colors.scrim.ignoresSafeArea()
            VStack(spacing: 0) {
                ShareFlightView(state: state, freezeT: freezeT, freezeHandoff: freezeHandoff, onlineAt: onlineAt, waitAt: waitAt)
                // The waiting text and the status line cross-fade in 150 ms.
                ZStack {
                    if waiting {
                        Text(state.statusText)
                            .font(.system(size: 13)).foregroundStyle(colors.text)
                            .multilineTextAlignment(.center).lineSpacing(4.5)
                            .frame(width: 240)
                            .fixedSize(horizontal: false, vertical: true)
                            .transition(still ? .identity : .opacity.animation(.linear(duration: 0.15)))
                    } else {
                        Text(state.statusText)
                            .font(.system(size: 13).monospacedDigit()).foregroundStyle(state.phase == .failed ? oink.badInk : colors.text)
                            .lineLimit(1).truncationMode(.tail)
                            .frame(width: 240, height: 20)
                            .transition(still ? .identity : .opacity.animation(.linear(duration: 0.15)))
                    }
                }
                .padding(.top, 12)
                // No bar while waiting, not even its track; it fades in during the hand-over.
                if !waiting {
                    ShareProgressBar(progress: state.phase == .succeeded ? 1 : state.progress,
                                     freezeT: freezeT ?? freezeHandoff.map { ShareAnim.handoff($0).t ?? 0 },
                                     failed: state.phase == .failed, onlineAt: onlineAt)
                        .opacity(freezeHandoff.map { Double(ShareAnim.handoff($0).bar) } ?? 1)
                        .padding(.top, 12)
                        .transition(still ? .identity : .opacity.animation(.linear(duration: 0.15)))
                }
                // Closing the extension would end a phone-to-phone transfer (review 2026-11-11).
                if state.toPhone && (state.phase == .preparing || state.phase == .sending) {
                    Text(AppText.pick("传完前请不要关闭此页面", "Keep this open until it finishes"))
                        .font(.system(size: 12)).foregroundStyle(oink.muted)
                        .multilineTextAlignment(.center)
                        .padding(.top, 10)
                }
                content()
            }
            .padding(.vertical, 20).padding(.horizontal, 16)
            .frame(width: 272)
            .background(colors.surface, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
            .overlay { RoundedRectangle(cornerRadius: 20, style: .continuous).strokeBorder(colors.border, lineWidth: 1) }
            .shadow(color: .black.opacity(oink.dark ? 0.3 : 0.06), radius: 1, y: 1)
            .shadow(color: oink.dark ? .black.opacity(0.4) : Color(red: 20 / 255, green: 22 / 255, blue: 28 / 255).opacity(0.16), radius: 16, y: 12)
            // 226 (waiting) ↔ 224 (sending) in 200 ms.
            .animation(still ? nil : .timingCurve(0.2, 0, 0, 1, duration: 0.2), value: waiting)
        }
        .onAppear { if waiting { announceWaiting() } }
        .onChange(of: state.phase) { old, new in
            guard freezeT == nil, freezeHandoff == nil else { return }
            let moving: Set<ShareUiState.Phase> = [.preparing, .sending]
            if new == .waiting {
                announceWaiting()
                onlineAt = nil
                guard !reduceMotion, moving.contains(old) else { return }
                let began = Date()
                waitAt = began
                // The entry lasts 150 ms; then the stage stops its time source.
                Task { @MainActor in
                    try? await Task.sleep(for: .milliseconds(Int(ShareAnim.enterWaitMs) + 10))
                    if waitAt == began { waitAt = nil }
                }
            } else if old == .waiting && moving.contains(new) {
                waitAt = nil
                if !reduceMotion { onlineAt = Date() }
            }
        }
    }
    /// The waiting text is read once when the wait begins (SPEC §2.1).
    private func announceWaiting() {
        UIAccessibility.post(notification: .announcement, argument: ShareUiState.waitingText)
    }
}

#if DEBUG
/// The example host app behind the card in the share design states (mockup「图库」).
struct ShareDemoHost: View {
    @Environment(\.oink) private var oink
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("图库").font(.system(size: 28, weight: .bold)).foregroundStyle(oink.ink)
                .frame(height: 34).padding(.horizontal, 4).padding(.top, 14).padding(.bottom, 12)
            LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: 3), count: 3), spacing: 3) {
                ForEach(0..<18, id: \.self) { index in
                    Rectangle().fill(index % 3 == 1 ? oink.shareTile2 : oink.shareTile)
                        .aspectRatio(1, contentMode: .fit)
                        .overlay { if index == 4 { Rectangle().strokeBorder(oink.systemApp, lineWidth: 3) } }
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 16)
        .background(oink.bg.ignoresSafeArea())
    }
}
#endif
