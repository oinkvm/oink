import SwiftUI
import UIKit

// Small controls shared by the app and the share extension (moved from
// App/UI/Components.swift; the extension needs them for its device picker).

enum Haptics {
    static func selection() { UISelectionFeedbackGenerator().selectionChanged() }
    static func long() { UIImpactFeedbackGenerator(style: .medium).impactOccurred() }
    static func success() { UINotificationFeedbackGenerator().notificationOccurred(.success) }
    static func error() { UINotificationFeedbackGenerator().notificationOccurred(.error) }
}

/// iOS switch of the mockup (`.sp-switch-ios`): 51 × 31 track, 27 thumb, accent when on.
/// Visual only — the surrounding row is the control.
struct OinkSwitch: View {
    @Environment(\.oink) private var oink
    let isOn: Bool
    var disabled = false
    var body: some View {
        ZStack(alignment: isOn ? .trailing : .leading) {
            Capsule().fill(isOn ? oink.accent : oink.track)
            Circle().fill(oink.thumb)
                .frame(width: 27, height: 27)
                .shadow(color: .black.opacity(0.15), radius: 4, y: 3)
                .shadow(color: .black.opacity(0.16), radius: 0.5, y: 1)
                .padding(2)
        }
        .frame(width: 51, height: 31)
        .opacity(disabled ? 0.38 : 1)
        .animation(.spring(response: 0.25, dampingFraction: 0.9), value: isOn)
        .accessibilityHidden(true)
    }
}

/// Spinner (`Spinner`): a 30 % arc, stroke 2.6 / 24, turning every 0.9 s.
struct OinkSpinner: View {
    @Environment(\.oink) private var oink
    var size: CGFloat = 20
    var color: Color?
    @State private var turning = false
    var body: some View {
        Circle()
            .trim(from: 0, to: 0.3)
            .stroke(color ?? oink.accent, style: StrokeStyle(lineWidth: size * 2.6 / 24, lineCap: .round))
            .padding(size * 2.5 / 24)
            .frame(width: size, height: size)
            .rotationEffect(.degrees(turning ? 360 : 0))
            .onAppear { withAnimation(.linear(duration: 0.9).repeatForever(autoreverses: false)) { turning = true } }
            .accessibilityHidden(true)
    }
}

/// Three dots blinking in turn (`Dots`).
struct OinkDots: View {
    var body: some View {
        TimelineView(.animation) { clock in
            let t = clock.date.timeIntervalSinceReferenceDate
            HStack(spacing: 3) {
                ForEach(0..<3, id: \.self) { i in
                    Circle().frame(width: 4, height: 4).opacity(Self.opacity(t - Double(i) * 0.15))
                }
            }
        }
        .accessibilityHidden(true)
    }
    /// 0 %, 80 %, 100 % → .25; 40 % → 1 over 1.2 s (ease-in-out).
    static func opacity(_ t: Double) -> Double {
        let p = (t.truncatingRemainder(dividingBy: 1.2) + 1.2).truncatingRemainder(dividingBy: 1.2) / 1.2
        let x = p < 0.4 ? p / 0.4 : p < 0.8 ? 1 - (p - 0.4) / 0.4 : 0
        let eased = x * x * (3 - 2 * x)
        return 0.25 + 0.75 * eased
    }
}

/// Capsule buttons: primary = soft accent fill + accent text; soft = grey chip;
/// danger = the primary capsule in red (`--c-bad-soft` fill, `--c-bad-ink` text).
struct CapsuleButtonStyle: ButtonStyle {
    enum Kind { case primary, soft, danger }
    @Environment(\.oink) private var oink
    @Environment(\.isEnabled) private var enabled
    var kind: Kind = .primary
    var fullWidth = false
    func makeBody(configuration: Configuration) -> some View {
        let primary = kind != .soft
        configuration.label
            .font(.oink(primary ? 15 : 14, primary ? .semibold : .medium))
            .foregroundStyle(kind == .primary ? oink.accentInk : kind == .danger ? oink.badInk : oink.text2)
            .padding(.horizontal, primary ? 24 : 22)
            .frame(maxWidth: fullWidth ? .infinity : nil)
            .frame(height: primary ? 48 : 44)
            .background(kind == .primary ? oink.accentSoft : kind == .danger ? oink.badSoft : oink.chip, in: Capsule())
            .opacity(enabled ? 1 : 0.4)
            .scaleEffect(configuration.isPressed && enabled ? 0.97 : 1)
            .animation(.easeOut(duration: 0.15), value: configuration.isPressed)
    }
}

/// Row press feedback: 5 % ink tint while pressed.
struct RowButtonStyle: ButtonStyle {
    @Environment(\.oink) private var oink
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .contentShape(Rectangle())
            .background(configuration.isPressed ? oink.press : .clear)
    }
}

/// Round buttons shrink to 0.9 while pressed.
struct ShrinkButtonStyle: ButtonStyle {
    var scale: CGFloat = 0.9
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? scale : 1)
            .animation(.easeOut(duration: 0.15), value: configuration.isPressed)
    }
}

/// Toast (`.sp-toast`): capsule at the top, optional action.
struct ToastModel: Equatable {
    var id = UUID()
    var text: String
    var action: String?
    static func == (a: ToastModel, b: ToastModel) -> Bool { a.id == b.id }
}

struct ToastView: View {
    @Environment(\.oink) private var oink
    let toast: ToastModel
    var onAction: () -> Void = {}
    var body: some View {
        HStack(spacing: 14) {
            Text(toast.text)
                .font(.oink(13))
                .foregroundStyle(oink.toastInk)
                .padding(.trailing, toast.action == nil ? 8 : 0)
            if let action = toast.action {
                Button(action: onAction) {
                    Text(action).font(.oink(13, .semibold))
                        .foregroundStyle(oink.dark ? oink.accentDeep : oink.accent)
                        .padding(.horizontal, 12).frame(height: 30)
                        .background(oink.toastActionBG, in: Capsule())
                }
                .buttonStyle(.plain)
            }
        }
        .padding(.leading, 16).padding(.trailing, 8).padding(.vertical, 8)
        .frame(minHeight: 42)
        .background(oink.toast, in: Capsule())
        .shadow(color: oink.shadow, radius: 8, y: 4)
        .transition(.move(edge: .top).combined(with: .opacity))
    }
}

/// Toast state with the mockup timing (2 s, or 4 s with an action).
@MainActor
final class ToastCenter: ObservableObject {
    @Published private(set) var toast: ToastModel?
    private var actionHandler: (() -> Void)?
    private var timer: Task<Void, Never>?
    func show(_ text: String, action: String? = nil, duration: Double? = nil, onAction: (() -> Void)? = nil) {
        timer?.cancel()
        let model = ToastModel(text: text, action: action)
        withAnimation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.26)) { toast = model }
        actionHandler = onAction
        let wait = duration ?? (action == nil ? 2 : 4)
        timer = Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(wait))
            guard !Task.isCancelled, self?.toast?.id == model.id else { return }
            withAnimation(.easeOut(duration: 0.2)) { self?.toast = nil }
        }
    }
    func act() {
        let handler = actionHandler
        timer?.cancel(); toast = nil; actionHandler = nil
        handler?()
    }
    func hide() { timer?.cancel(); toast = nil }
}

/// Download / receive progress ring (`.tr-progress`): r 15 of 36, stroke 2.5, number inside.
struct ProgressRing: View {
    @Environment(\.oink) private var oink
    let value: Int
    var size: CGFloat = 36
    var trackColor: Color?
    var body: some View {
        ZStack {
            Circle().stroke(trackColor ?? oink.progressTrack, lineWidth: size * 2.5 / 36)
            Circle().trim(from: 0, to: CGFloat(value) / 100)
                .stroke(oink.accent, style: StrokeStyle(lineWidth: size * 2.5 / 36, lineCap: .round))
                .rotationEffect(.degrees(-90))
            Text("\(value)").font(.oink(10, .semibold).monospacedDigit()).foregroundStyle(oink.muted)
        }
        .padding(size * 3 / 36)
        .frame(width: size, height: size)
        .accessibilityElement()
        .accessibilityLabel("进度")
        .accessibilityValue("\(value)%")
    }
}

/// Group heading with a count chip (`.m-group h3`).
struct GroupHeading<Trailing: View>: View {
    @Environment(\.oink) private var oink
    let title: String
    var count: Int?
    @ViewBuilder var trailing: () -> Trailing
    var body: some View {
        HStack(spacing: 8) {
            Text(title).font(.oink(13, .semibold)).kerning(0.4).foregroundStyle(oink.muted)
            if let count {
                Text("\(count)").font(.oink(11, .semibold).monospacedDigit()).foregroundStyle(oink.text2)
                    .frame(minWidth: 20, minHeight: 20)
                    .background(oink.chip, in: RoundedRectangle(cornerRadius: 7, style: .continuous))
            }
            Spacer(minLength: 0)
            trailing()
        }
        .padding(.leading, 4)
    }
}

extension GroupHeading where Trailing == EmptyView {
    init(title: String, count: Int?) { self.init(title: title, count: count, trailing: { EmptyView() }) }
}

/// 1 px separator in the line colour.
struct Hairline: View {
    @Environment(\.oink) private var oink
    var color: Color?
    var body: some View { Rectangle().fill(color ?? oink.line).frame(height: 1) }
}

