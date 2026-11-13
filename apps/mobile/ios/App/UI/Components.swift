import SwiftUI
import UIKit

// Shared building blocks of the mockup (`ui-kit.jsx`, `settings-pairing.css`).

/// Control Center style glass on sheets (iOS 26 Liquid Glass; a thin material before).
struct OinkGlass: ViewModifier {
    var enabled = true
    var interactive = false
    var tint: Color?
    let shape: AnyShape
    func body(content: Content) -> some View {
        if !enabled {
            content
        } else if #available(iOS 26.0, *) {
            content.glassEffect(Self.glass(interactive: interactive, tint: tint), in: shape)
        } else {
            content.background { shape.fill(.ultraThinMaterial).overlay(shape.fill(tint ?? .clear)) }
        }
    }
    @available(iOS 26.0, *)
    private static func glass(interactive: Bool, tint: Color?) -> Glass {
        var glass = Glass.regular
        if let tint { glass = glass.tint(tint) }
        return interactive ? glass.interactive() : glass
    }
}

extension View {
    func oinkGlass<S: Shape>(in shape: S, interactive: Bool = false, tint: Color? = nil, enabled: Bool = true) -> some View {
        modifier(OinkGlass(enabled: enabled, interactive: interactive, tint: tint, shape: AnyShape(shape)))
    }
}

/// Inactive control on a sheet (user rule 2026-11-08): Liquid Glass with a white icon.
/// In light mode the glass gets a dark system tint so white stays legible on the light sheet.
struct SheetGlass: ViewModifier {
    @Environment(\.oink) private var oink
    let shape: AnyShape
    var interactive = true
    func body(content: Content) -> some View {
        content.modifier(OinkGlass(interactive: interactive, tint: oink.dark ? Color.white.opacity(0.14) : Color.black.opacity(0.34), shape: shape))
            // The glass effect does not count for hit testing: without this a glass capsule
            // button only reacts on its label glyphs (the failed-pairing「关闭」, 2026-11-09).
            .contentShape(shape)
    }
}

extension View {
    func sheetGlass<S: Shape>(in shape: S, interactive: Bool = true) -> some View {
        modifier(SheetGlass(shape: AnyShape(shape), interactive: interactive))
    }
}

/// Half-sheet sizing (user 2026-11-08): the content sits inside the floating sheet with the
/// same margin on all four sides, and the sheet is exactly as tall as that. The sheet keeps the
/// system's floating style, whose corners are concentric with the display corners.
struct FittedSheet: ViewModifier {
    static let margin: CGFloat = 24
    var dragIndicator: Visibility = .visible
    /// Space above and below the content (the sides always keep `margin`).
    var top = margin
    var bottom = margin
    @State private var height: CGFloat = 320
    /// The window's bottom safe area (home indicator), which the floating sheet adds under a
    /// `.height` detent.
    @State private var windowInset: CGFloat = 0
    func body(content: Content) -> some View {
        // The page hangs from the top of a flexible view: the sheet's hosting view is never
        // shorter than its root's ideal height and stays pinned to the sheet bottom, so a root
        // as tall as the content would stick out above the glass once the detent is smaller.
        Color.clear
            .overlay(alignment: .top) {
                content
                    .padding(.horizontal, Self.margin)
                    .padding(.top, top)
                    .padding(.bottom, bottom)
                    .fixedSize(horizontal: false, vertical: true)
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height = $0 }
            }
            // A `.height` detent is the height above the window's bottom safe area, and the
            // floating sheet (iOS 26+) still draws its glass over that safe area under it: on
            // iPhone Air (iOS 27 simulator) the glass was the detent + 34 pt, all of it below the
            // content, so the bottom margin was 58 pt against 24 at the top. The page therefore
            // fills the whole glass (it ignores the bottom safe area, which lies inside the
            // floating sheet, clear of the home indicator) and the detent leaves out the window
            // inset. Read from the window, not this view: inside the sheet the inset depends on
            // the detent itself.
            .onGeometryChange(for: CGSize.self) { $0.size } action: { _ in windowInset = Self.windowBottomInset() }
            .ignoresSafeArea(.container, edges: .bottom)
            .presentationDetents([.height(max(height - windowInset, 1))])
            .presentationDragIndicator(dragIndicator)
            .onAppear { windowInset = Self.windowBottomInset() }
    }

    private static func windowBottomInset() -> CGFloat {
        let windows = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.flatMap(\.windows)
        return (windows.first(where: \.isKeyWindow) ?? windows.first)?.safeAreaInsets.bottom ?? 0
    }
}

extension View {
    func fittedSheet(dragIndicator: Visibility = .visible, top: CGFloat = FittedSheet.margin,
                     bottom: CGFloat = FittedSheet.margin) -> some View {
        modifier(FittedSheet(dragIndicator: dragIndicator, top: top, bottom: bottom))
    }
}

/// The six one-time-code cells (`CodeCells`). One hidden numeric field drives six boxes;
/// `showOnly` draws the digits without boxes (the shown code, same geometry).
struct CodeCells: View {
    @Environment(\.oink) private var oink
    @Binding var value: String
    var invalid = false
    var disabled = false
    var success = false
    var showOnly = false
    /// `--c-fill` by default; the pairing sheet uses `--c-elev` (mockup `.ad-pair-sheet .sp-code-cell`).
    var cellFill: Color?
    var onComplete: (String) -> Void = { _ in }
    @FocusState private var focused: Bool

    var body: some View {
        ZStack {
            if !showOnly {
                TextField("", text: Binding(get: { value }, set: { new in
                    let digits = String(new.filter(\.isNumber).prefix(6))
                    guard digits != value else { return }
                    value = digits
                    if digits.count == 6 { onComplete(digits) }
                }))
                .keyboardType(.numberPad)
                .textContentType(.oneTimeCode)
                .focused($focused)
                .disabled(disabled)
                // The field only takes the input; the cells draw the digits. Its own text, caret
                // and selection are clear: at a low opacity alone the typed digits still showed as
                // a faint ghost left of the first cell on the sheet glass. The small opacity stays
                // so the field is still a focusable, hit-testable element.
                .foregroundStyle(.clear)
                .tint(.clear)
                .opacity(0.02)
                .accessibilityLabel("配对码")
            }
            HStack(spacing: 6) {
                ForEach(0..<6, id: \.self) { i in cell(i).padding(.leading, i == 3 ? 8 : 0) }
            }
            .allowsHitTesting(false)
        }
        .contentShape(Rectangle())
        .onTapGesture { if !showOnly && !disabled { focused = true } }
        .onAppear { if !showOnly && !disabled && DesignScene.current == nil { focused = true } }
        .onChange(of: disabled) { _, now in if !now && !showOnly { focused = true } }
    }

    private func cell(_ i: Int) -> some View {
        let chars = Array(value)
        let digit = i < chars.count ? String(chars[i]) : ""
        let active = !showOnly && !disabled && !invalid && i == min(chars.count, 5)
        let border: Color = showOnly ? .clear : invalid ? oink.bad : active ? oink.accent : oink.track
        let ink: Color = success ? oink.goodInk : invalid ? oink.badInk : disabled ? oink.muted : oink.ink
        return Text(digit)
            .font(.oink(26, .semibold).monospacedDigit())
            .foregroundStyle(ink)
            .accessibilityIdentifier("pairing-code-digit")
            .frame(width: 42, height: 54)
            .background(showOnly ? .clear : (cellFill ?? oink.fill), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).strokeBorder(border, lineWidth: 1.5))
            .background {
                if active { RoundedRectangle(cornerRadius: 14, style: .continuous).stroke(oink.accentSoft, lineWidth: 6) }
            }
    }
}

/// Horizontal shake of the invalid inputs (520 ms).
struct ShakeEffect: GeometryEffect {
    var travel: CGFloat
    var animatableData: CGFloat {
        get { travel }
        set { travel = newValue }
    }
    func effectValue(size: CGSize) -> ProjectionTransform {
        let phase = travel - travel.rounded(.down)
        let keys: [CGFloat] = [0, -7, 6, -4, 2, 0]
        let pos = phase * CGFloat(keys.count - 1)
        let i = min(Int(pos), keys.count - 2)
        let x = keys[i] + (keys[i + 1] - keys[i]) * (pos - CGFloat(i))
        return ProjectionTransform(CGAffineTransform(translationX: travel == travel.rounded(.down) ? 0 : x, y: 0))
    }
}
