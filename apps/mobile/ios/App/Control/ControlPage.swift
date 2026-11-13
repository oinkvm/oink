import SwiftUI
import UIKit

// 控制 tab: the phone as a touchpad and keyboard for the focus computer
// (docs control-page.md). Portrait: header with 调节 menu, focus capsule, lock / game button and
// a slot that shows 切换键盘 while a keyboard is up; the touchpad slab below; the keyboard
// (hidden field + input views) rises from the bottom and pushes the pad up; it opens from the
// keyboard button beside the tab bar (ControlTabBar). Landscape (§1.2): a 56 top bar with the tabs
// in it, and the keyboard in one half of the screen (ControlLandscapeLayout).

struct ControlTab: View {
    @EnvironmentObject private var core: CoreSession
    @EnvironmentObject private var toasts: ToastCenter
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.oink) private var oink
    @ObservedObject var model: ControlModel
    let isActive: Bool
    let onSelectTab: (HomeTab) -> Void

    private var live: Bool { ControlPageExit.keepsInput(tabShown: isActive, phase: Self.phase(scenePhase)) }

    var body: some View {
        ControlPage(model: model, onSelectTab: onSelectTab)
            // Over the tab view (MainView): opaque and hit-testable to the screen edges, so nothing of
            // the system tab bar under it shows or takes a touch.
            .background(oink.bg.ignoresSafeArea())
            .onChange(of: LiveKey(live: live, revision: core.sessionRevision), initial: true) { _, key in
                #if DEBUG
                if DesignScene.hasPrefix("control") { ControlScenes.install(model); return }
                #endif
                if key.live { model.activate(core: core) } else { model.deactivate() }
            }
            .onChange(of: model.noTargetPrompt) { _, _ in toasts.show("请先选择要控制的电脑") }
            .onChange(of: model.errorText) { _, _ in if let text = model.consumeError() { toasts.show(text) } }
            // Background (not merely inactive: Control Center, Notification Center, an alert) is
            // leaving the page for 全屏 / 恢复 (user 2026-11-12).
            .onChange(of: scenePhase) { _, phase in
                if ControlPageExit.left(tabShown: true, phase: Self.phase(phase)) { model.leftPage() }
            }
            // Leaving the control tab removes the page: everything held on the remote goes up, and
            // the full screen ends.
            .onDisappear {
                model.deactivate()
                model.leftPage()
            }
    }

    private static func phase(_ phase: ScenePhase) -> ControlPageExit.Phase {
        switch phase {
        case .background: .background
        case .inactive: .inactive
        default: .active
        }
    }

    private struct LiveKey: Equatable { var live: Bool; var revision: Int }
}

/// The 调节 menu's open state is the model's (`adjustOpen`, drawn by AdjustLayer) so the 调节 button
/// in the header and the menu layer share it without the page in between.
struct ControlPage: View {
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.verticalSizeClass) private var sizeClass
    @ObservedObject var model: ControlModel
    var onSelectTab: (HomeTab) -> Void = { _ in }
    /// The home-indicator inset under the page, keyboard excluded (measured below).
    @State private var home: CGFloat = 0
    /// iPhone landscape. iPads keep the portrait layout in both orientations.
    private var landscape: Bool { sizeClass == .compact }

    var body: some View {
        Group {
            if landscape {
                LandscapeControl(model: model, home: home) { header(landscape: true) }
            } else {
                VStack(spacing: 0) {
                    header(landscape: false)
                        .frame(height: 64)
                        .background(oink.bg)
                        .zIndex(6)
                    // The page follows the keyboard safe area (the system animates it with the
                    // keyboard's own duration and curve); the pad keeps 12 above the keyboard or the
                    // tab bar.
                    TrackpadArea(model: model)
                        .padding(.horizontal, 16)
                        .padding(.top, 2)
                        .padding(.bottom, 12)
                    // The tab bar and the keyboard button; they slide away while a keyboard is up.
                    ControlTabBar(model: model, onSelect: onSelectTab, home: home)
                }
            }
        }
        .background(oink.bg)
        // The bottom safe-area inset without the keyboard: a layer that ignores the keyboard's safe
        // area reports only the home indicator's. Measured at layout time, never read from UIKit in
        // a body (see HomeInset).
        .background {
            Color.clear
                .ignoresSafeArea(.keyboard)
                .onGeometryChange(for: CGFloat.self) { $0.safeAreaInsets.bottom } action: { home = $0 }
        }
        .background { ControlKeyboardHost(model: model, palette: oink, landscape: landscape).frame(width: 1, height: 1).opacity(0.02) }
        .overlay(alignment: .topLeading) { AdjustLayer(model: model) }
        // The keyboard choice is per orientation (ControlModel.setLandscape).
        .onChange(of: landscape, initial: true) { _, value in model.setLandscape(value) }
    }

    private var adjustButton: some View {
        ToolbarIconButton(label: "调节", action: { [model, reduceMotion] in
            Haptics.selection()
            withAnimation(reduceMotion ? nil : .timingCurve(0.22, 1, 0.36, 1, duration: 0.26)) { model.adjustOpen.toggle() }
        }) {
            Lucide(name: "sliders-horizontal", size: 23, color: oink.ink)
        }
    }

    /// Portrait (64): 调节, the focus capsule and the lock centred, the top-right slot. Landscape
    /// (the floating bar's 48 row, user 2026-11-12): three columns — 调节 left; the tabs, the
    /// capsule and the lock as one group centred on the screen; the keyboard button and 恢复 right.
    /// Every item is on the tab bar's surface (Liquid Glass), 调节 / keyboard / 恢复 as 48 circles.
    @ViewBuilder private func header(landscape: Bool) -> some View {
        if landscape {
            ZStack {
                HStack(spacing: 0) {
                    GlassCircleButton(label: "调节", action: { [model, reduceMotion] in
                        Haptics.selection()
                        withAnimation(reduceMotion ? nil : .timingCurve(0.22, 1, 0.36, 1, duration: 0.26)) { model.adjustOpen.toggle() }
                    }) {
                        Lucide(name: "sliders-horizontal", size: 22, color: oink.ink)
                    }.controlBarHitArea("adjust")
                    Spacer(minLength: 0)
                    HStack(spacing: 10) {
                        KeyboardToggleButton(model: model).controlBarHitArea("keyboard")
                        GlassCircleButton(label: "恢复", action: { [model] in
                            Haptics.selection()
                            model.restoreFromFullscreen()
                        }) {
                            Image(systemName: "arrow.down.right.and.arrow.up.left").font(.system(size: 18, weight: .medium)).foregroundStyle(oink.ink)
                        }.controlBarHitArea("restore")
                    }
                }
                HStack(spacing: 10) {
                    LandscapeTabs(onSelect: onSelectTab).controlBarHitArea("tabs")
                    FocusCapsule(model: model, metrics: .landscape).controlBarHitArea("focus")
                    LockButton(model: model, diameter: 48).controlBarHitArea("lock")
                }
                .padding(.horizontal, 48 + 10 + 48 + 8)
            }
        } else {
            HStack(spacing: 4) {
                adjustButton
                HStack(spacing: 8) {
                    FocusCapsule(model: model, metrics: .portrait)
                    LockButton(model: model, diameter: 40)
                }
                .frame(maxWidth: .infinity)
                KeyboardSwitchSlot(model: model)
            }
            .padding(.horizontal, 16)
        }
    }
}

/// HomeInset: why the page measures the home-indicator inset instead of asking the window.
///
/// Reading `keyWindow.safeAreaInsets` inside a view's `body` (a helper did, from 7d5d81e28 on)
/// stopped SwiftUI from ever running that body again: with the read in ControlTabBar's body, the
/// body ran twice at launch and never after, though it observes the model — the keyboard opened
/// and the bar stayed (os_log of the body's runs on the simulator, 2026-11-12); without the read it
/// ran on every keyboard change. The same read in ControlPage's body (7d5d81e28) is what froze the
/// page then: the 调节 menu (page @State) never showed, the bar never left the start of its entry
/// animation, the control-adjust scene's onAppear state never appeared — every symptom of the
/// integration rounds 400004 / 400005 that followed.

// MARK: Portrait tab bar

/// Portrait bottom: 274 × 62 tabs and a 62 keyboard button, gap 8, centred as one group.
/// The page covers TabView's native bar to avoid its input container intercepting these controls
/// (iOS 26 integration 400004/400005). ControlArrival animates entry/exit from the native bar's
/// centre without onAppear state or delayed callbacks. Keyboard opening still removes both.
private struct ControlTabBar: View {
    @Environment(\.oink) private var oink
    @Environment(\.controlBarArrival) private var arrival
    @ObservedObject var model: ControlModel
    let onSelect: (HomeTab) -> Void
    /// The home-indicator inset (HomeInset).
    let home: CGFloat

    static let width: CGFloat = 274
    static let height: CGFloat = 62

    /// Space between the bar and the screen bottom: 21 over the home indicator, 8 on a phone
    /// without one.
    static func bottomGap(home: CGFloat) -> CGFloat { home > 0 ? 21 : 8 }

    /// Shown exactly while no keyboard (of either kind) is open — a function of the current state.
    /// (It stayed over the keyboard in integration 400005 because this body read the window's safe
    /// area and so never ran again; see HomeInset.)
    var body: some View {
        let shown = !model.keyboardOpen
        Group {
            if shown {
                bar
                    // Into the home-indicator strip: 21 above the screen bottom.
                    .padding(.bottom, Self.bottomGap(home: home) - home)
            }
        }
        .transaction { $0.animation = nil }
    }

    private var bar: some View {
        HStack(spacing: PortraitControlBarMetrics.gap) {
            tabs.offset(x: (1 - arrival) * PortraitControlBarMetrics.tabTravel)
            keyboardButton
                .scaleEffect(max(0.01, arrival))
                .opacity(arrival)
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, 16)
        .frame(height: Self.height)
    }

    private var tabs: some View {
        HStack(spacing: 0) {
            tab("设备", icon: "Lucide-monitor", .devices)
            tab("传输", icon: "Lucide-arrow-down-up", .transfer)
            tab("控制", icon: "Tab-control", .control)
        }
        .padding(4)
        .frame(width: Self.width, height: Self.height)
        .oinkGlass(in: Capsule())
        .contentShape(Capsule())
        .accessibilityElement(children: .contain)
        .accessibilityAddTraits(.isTabBar)
    }

    private func tab(_ title: String, icon: String, _ value: HomeTab) -> some View {
        let selected = value == .control
        return Button {
            Haptics.selection()
            guard !selected else { return }
            onSelect(value)
        } label: {
            VStack(spacing: 2) {
                Image("\(icon)-\(selected ? "2_2" : "1_8")").renderingMode(.template).resizable().scaledToFit()
                    .frame(width: 24, height: 24)
                Text(title).font(.oink(10, .medium)).lineLimit(1)
            }
            .accessibilityHidden(true)
            .foregroundStyle(selected ? oink.accentInk : oink.ink)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(selected ? oink.chip : .clear, in: Capsule())
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        // One element per tab (its icon and title are not separate buttons).
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(title)
        .accessibilityAddTraits(selected ? [.isButton, .isSelected] : .isButton)
        .accessibilityAction { Haptics.selection(); if !selected { onSelect(value) } }
        .accessibilityIdentifier(ControlTabID.of(value))
    }

    private var keyboardButton: some View {
        Button {
            Haptics.selection()
            model.openKeyboard()
        } label: {
            AssetIcon("PCGlyph-keyboard", size: 26, color: oink.ink)
                .frame(width: Self.height, height: Self.height)
                .oinkGlass(in: Circle(), interactive: true)
                .contentShape(Circle())
        }
        .buttonStyle(ShrinkButtonStyle())
        .accessibilityLabel("弹出键盘")
        .accessibilityIdentifier(ControlTabID.keyboard)
    }
}

/// Accessibility identifiers of the page's own tabs (portrait bar and landscape top bar; only one
/// is on screen) and of the keyboard button beside the portrait bar: one element each, so a query
/// never lands on a tab's icon or title, or on the system tab bar under the page.
enum ControlTabID {
    static let keyboard = "control-tab-keyboard"
    static func of(_ tab: HomeTab) -> String {
        switch tab {
        case .devices: "control-tab-devices"
        case .transfer: "control-tab-transfer"
        case .control: "control-tab-control"
        }
    }
}

// MARK: Landscape

/// Landscape tabs in the top bar (用户 2026-11-12): the tab bar's three tabs on its surface, 48
/// high, icon 20 and text 13 side by side; the current one (控制) on a grey capsule in the theme
/// colour.
private struct LandscapeTabs: View {
    @Environment(\.oink) private var oink
    let onSelect: (HomeTab) -> Void

    var body: some View {
        HStack(spacing: 2) {
            tab("设备", icon: "Lucide-monitor", .devices)
            tab("传输", icon: "Lucide-arrow-down-up", .transfer)
            tab("控制", icon: "Tab-control", .control)
        }
        .padding(4)
        .frame(height: 48)
        .oinkGlass(in: Capsule())
        .accessibilityElement(children: .contain)
    }

    private func tab(_ title: String, icon: String, _ value: HomeTab) -> some View {
        let selected = value == .control
        return Button {
            Haptics.selection()
            guard !selected else { return }
            onSelect(value)
        } label: {
            HStack(spacing: 6) {
                Image("\(icon)-\(selected ? "2_2" : "1_8")").renderingMode(.template).resizable().scaledToFit()
                    .frame(width: 20, height: 20)
                if selected { Text(title).font(.oink(13, .medium)).fixedSize() }
            }
            .accessibilityHidden(true)
            .foregroundStyle(selected ? oink.accentInk : oink.text2)
            .padding(.leading, 12)
            .padding(.trailing, 14)
            .frame(height: 40)
            .background(selected ? oink.chip : .clear, in: Capsule())
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        // One element per tab (its icon and title are not separate buttons).
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(title)
        .accessibilityAddTraits(selected ? [.isButton, .isSelected] : .isButton)
        .accessibilityAction { Haptics.selection(); if !selected { onSelect(value) } }
        .accessibilityIdentifier(ControlTabID.of(value))
    }
}

/// Which way round the phone is held (§1.2). Both landscape grips have the same size and safe
/// areas, so the page reads the scene's interface orientation (KVO on `effectiveGeometry`).
struct InterfaceOrientationReader: UIViewRepresentable {
    let onChange: (UIInterfaceOrientation) -> Void

    func makeUIView(context: Context) -> ReaderView { ReaderView(onChange: onChange) }
    func updateUIView(_ view: ReaderView, context: Context) { view.onChange = onChange }

    final class ReaderView: UIView {
        var onChange: (UIInterfaceOrientation) -> Void
        private var observation: NSKeyValueObservation?

        init(onChange: @escaping (UIInterfaceOrientation) -> Void) {
            self.onChange = onChange
            super.init(frame: .zero)
            isUserInteractionEnabled = false
            isAccessibilityElement = false
        }
        required init?(coder: NSCoder) { fatalError() }

        override func didMoveToWindow() {
            super.didMoveToWindow()
            observation = nil
            guard let scene = window?.windowScene else { return }
            observation = scene.observe(\.effectiveGeometry, options: [.initial, .new]) { [weak self] scene, _ in
                let orientation = scene.effectiveGeometry.interfaceOrientation
                // Not inside the update that changed the geometry.
                DispatchQueue.main.async { self?.onChange(orientation) }
            }
        }
    }
}

// MARK: Header pieces

/// Focus capsule (§2.2): platform logo + focus computer name + chevron; tap opens the list of
/// online computers that can be controlled (system menu). On iOS it has the tab bar's Liquid Glass
/// surface (用户 2026-11-12); its padding puts the logo as far from the left edge as the chevron
/// from the right (`FocusCapsuleMetrics`).
private struct FocusCapsule: View {
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @EnvironmentObject private var core: CoreSession
    /// The menu shows each computer's live latency, followed only while the page is shown:
    /// the latency changes about once a second, and rebuilding this menu each time cost
    /// 50–80 ms on the main thread even with another tab on screen (simulator).
    @ObservedObject private var liveLatency: CardLatency
    @ObservedObject var model: ControlModel
    let metrics: FocusCapsuleMetrics

    init(model: ControlModel, metrics: FocusCapsuleMetrics) {
        self.model = model
        self.metrics = metrics
        // The control design states never activate the model.
        let shown = model.active || DesignScene.hasPrefix("control")
        _liveLatency = ObservedObject(wrappedValue: shown ? CoreSession.shared.latency : CardLatency.idle)
    }

    var body: some View {
        let focus = model.focus
        Menu {
            Section("可控制的在线设备") {
                if model.controllable.isEmpty {
                    Text("没有在线的电脑")
                } else {
                    ForEach(model.controllable) { target in
                        Button {
                            Haptics.selection()
                            model.moveFocus(to: target.device)
                        } label: {
                            Label {
                                Text(target.name)
                                if let latency = latency(target.device) { Text("\(latency) ms") }
                            } icon: {
                                if target.device == model.snapshot.focusDevice {
                                    Image(systemName: "checkmark")
                                } else {
                                    Image(menuImage(target.platform)).renderingMode(.template)
                                }
                            }
                        }
                    }
                }
            }
        } label: {
            HStack(spacing: 6) {
                if let focus {
                    PlatformLogo(platform: DevicePlatform(os: focus.platform), scale: 0.62)
                        .frame(width: 26, height: 26)
                        .padding(.horizontal, -2)
                }
                Text(focus?.name ?? "选择设备")
                    .font(.oink(15, .semibold))
                    .foregroundStyle(oink.ink)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .id(focus?.device ?? "")
                    .transition(reduceMotion ? .identity : .asymmetric(insertion: .offset(y: 6).combined(with: .opacity), removal: .opacity))
                Lucide(name: "chevron-down", size: 16, color: oink.muted)
            }
            .padding(.leading, focus == nil ? metrics.leadingNoLogo : metrics.leading)
            .padding(.trailing, metrics.trailing)
            .frame(height: metrics.height)
            .oinkGlass(in: Capsule())
            .contentShape(Capsule())
            .animation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.2), value: focus?.device)
        }
        .menuStyle(.button)
        .buttonStyle(ShrinkButtonStyle())
        .simultaneousGesture(TapGesture().onEnded { Haptics.selection() })
        .accessibilityLabel(focus.map { "焦点设备：\($0.name)" } ?? "选择要控制的设备")
    }

    private func latency(_ device: String) -> Int? {
        liveLatency.ms[device]
    }

    private func menuImage(_ platform: String) -> String {
        switch DevicePlatform(os: platform) {
        case .windows: "MenuOS-win"
        case .mac: "MenuOS-mac"
        case .android: "MenuOS-android"
        default: "MenuOS-other"
        }
    }
}

/// Lock / game button (§2.3): tap toggles, long press opens the system menu. Game mode shows
/// only while it is on (on, or auto and on), with no AUTO tag (用户 2026-11-12); a tap on the
/// gamepad turns it off. The tab bar's Liquid Glass surface, the theme's light tint while on.
private struct LockButton: View {
    @Environment(\.oink) private var oink
    @ObservedObject var model: ControlModel
    let diameter: CGFloat

    var body: some View {
        let state = model.lockState
        let on = state != .unlocked
        Menu {
            Button {
                Haptics.selection()
                model.setPointerLock(state != .locked)
            } label: {
                Label { Text(state == .locked ? "解锁指针" : "锁定指针") } icon: {
                    MenuGlyph.image(state == .locked ? .unlock : .lock)
                }
            }
            Menu {
                Picker("游戏模式", selection: Binding(get: { model.snapshot.gameMode }, set: { Haptics.selection(); model.setGameMode($0) })) {
                    Text("自动").tag(ControlGame.auto)
                    Text("开").tag(ControlGame.on)
                    Text("关").tag(ControlGame.off)
                }
                .pickerStyle(.inline)
            } label: {
                Label {
                    Text("游戏模式")
                    Text(gameLabel(model.snapshot.gameMode))
                } icon: { MenuGlyph.image(.game) }
            }
        } label: {
            icon(state, color: on ? oink.accentInk : oink.text2)
                .frame(width: diameter, height: diameter)
                .background(on ? oink.accentSoft2 : .clear, in: Circle())
                .oinkGlass(in: Circle())
                .contentShape(Circle())
                .animation(.easeOut(duration: 0.2), value: on)
        } primaryAction: {
            Haptics.selection()
            model.lockTapped()
        }
        .menuStyle(.button)
        .buttonStyle(ShrinkButtonStyle())
        .accessibilityLabel(label(state))
        .accessibilityHint(state == .unlocked ? "点按锁定，长按更多" : state.game ? "点按关闭游戏模式，长按更多" : "点按解锁，长按更多")
    }

    @ViewBuilder private func icon(_ state: LockButtonState, color: Color) -> some View {
        switch state {
        case .unlocked: AssetIcon("PCAction-unlock", size: 21, color: color)
        case .locked: Lucide(name: "lock-keyhole", size: 20, color: color)
        case .gameOn, .gameAuto: AssetIcon("PCGlyph-game", size: 22, color: color)
        }
    }

    private func gameLabel(_ g: ControlGame) -> String { switch g { case .auto: "自动"; case .on: "开"; case .off: "关" } }

    private func label(_ state: LockButtonState) -> String {
        switch state {
        case .unlocked: "指针未锁定"
        case .locked: "指针已锁定"
        case .gameOn: "游戏模式已开启"
        case .gameAuto: "游戏模式已开启（自动）"
        }
    }
}

/// Icons of the lock button's long-press menu (用户 2026-11-12, second round): one 20 slot, the
/// lock 17 (unlock 18) and the gamepad 19, which look the same size. UIMenu draws an asset image
/// at its own size (the gamepad PDF came out visibly smaller than the lock), so each glyph is
/// rendered into the slot once, as a template image the menu tints.
@MainActor
private enum MenuGlyph: Hashable {
    case lock, unlock, game

    private static var cache: [MenuGlyph: UIImage] = [:]

    static func image(_ glyph: MenuGlyph) -> Image {
        if let cached = cache[glyph] { return Image(uiImage: cached) }
        let (name, size): (String, CGFloat) = switch glyph {
        case .lock: ("Lucide-lock-keyhole-2", 17)
        case .unlock: ("PCAction-unlock", 18)
        case .game: ("PCGlyph-game", 19)
        }
        let renderer = ImageRenderer(content: Image(name).renderingMode(.template).resizable().scaledToFit()
            .frame(width: size, height: size)
            .frame(width: 20, height: 20))
        renderer.scale = UITraitCollection.current.displayScale > 0 ? UITraitCollection.current.displayScale : 3
        let image = (renderer.uiImage ?? UIImage()).withRenderingMode(.alwaysTemplate)
        cache[glyph] = image
        return Image(uiImage: image)
    }
}

/// Landscape bar, left of 恢复 (§2.4): opens the keyboard (the bar hides while it is up). A 48
/// circle on the bar's surface; active while a keyboard is up.
private struct KeyboardToggleButton: View {
    @Environment(\.oink) private var oink
    @ObservedObject var model: ControlModel
    var body: some View {
        let on = model.keyboardOpen
        Button {
            Haptics.selection()
            model.toggleKeyboard()
        } label: {
            AssetIcon("PCGlyph-keyboard", size: 24, color: on ? oink.accentInk : oink.ink)
                .frame(width: 48, height: 48)
                .background(on ? oink.accentSoft2 : .clear, in: Circle())
                .oinkGlass(in: Circle())
                .contentShape(Circle())
        }
        .buttonStyle(ShrinkButtonStyle())
        .accessibilityLabel(on ? "收起键盘" : "弹出键盘")
        .accessibilityAddTraits(on ? .isSelected : [])
    }
}

/// A 48 circle on the floating bar's surface (调节, 恢复).
private struct GlassCircleButton<Icon: View>: View {
    let label: String
    let action: () -> Void
    @ViewBuilder var icon: () -> Icon
    var body: some View {
        Button(action: action) {
            icon()
                .frame(width: 48, height: 48)
                .oinkGlass(in: Circle())
                .contentShape(Circle())
        }
        .buttonStyle(ShrinkButtonStyle())
        .accessibilityLabel(label)
    }
}

/// Portrait top-right (§2.4, 用户 2026-11-12): 全屏 while no keyboard is up (two arrows pointing
/// out, ink, no background; enters the landscape full screen even with the phone upright), 切换键盘
/// while one is (the keyboard with the switch badge, 24); 48 wide either way so the capsule stays
/// centred.
private struct KeyboardSwitchSlot: View {
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// Phone-sized windows only: an iPad (regular width) keeps the portrait layout when turned, so
    /// it has no full screen to enter (as Android).
    @Environment(\.horizontalSizeClass) private var widthClass
    @ObservedObject var model: ControlModel

    var body: some View {
        ZStack {
            if model.keyboardOpen {
                Button {
                    Haptics.selection()
                    model.switchKeyboard()
                } label: {
                    AssetIcon("Control-switch-keyboard", size: 24, color: oink.ink)
                        .frame(width: 48, height: 48)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel(ControlKeysBar.switchLabel(system: model.shownKeyboard == .system))
                .transition(reduceMotion ? .opacity : .scale(scale: 0.7).combined(with: .opacity))
            } else if widthClass == .compact {
                Button {
                    Haptics.selection()
                    model.enterFullscreen()
                } label: {
                    Image(systemName: "arrow.up.left.and.arrow.down.right").font(.system(size: 19, weight: .medium))
                        .foregroundStyle(oink.ink)
                        .frame(width: 48, height: 48)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("全屏")
                .transition(reduceMotion ? .opacity : .scale(scale: 0.7).combined(with: .opacity))
            }
        }
        .frame(width: 48, height: 48)
        .animation(reduceMotion ? nil : .timingCurve(0.22, 1, 0.36, 1, duration: 0.2), value: model.keyboardOpen)
    }
}

// MARK: 调节 menu

/// 调节 while open (§2.1): a catcher over the whole screen that closes it, and the menu under the
/// top bar (58 from the page top; 52 in landscape). It observes the model's `adjustOpen` itself.
private struct AdjustLayer: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.verticalSizeClass) private var sizeClass
    @ObservedObject var model: ControlModel

    var body: some View {
        let landscape = sizeClass == .compact
        ZStack(alignment: .topLeading) {
            if model.adjustOpen {
                // Outside the menu: the first touch only closes it (it never reaches the touchpad).
                Color.clear.contentShape(Rectangle())
                    .onTapGesture { close() }
                    .ignoresSafeArea()
                // Portrait: under the 64 top bar. Landscape: 4 below the floating bar's 调节 button
                // (its row is 12 + 48 from the screen's top edge), left-aligned with it.
                AdjustMenu(model: model, landscape: landscape, onClose: close)
                    .padding(.top, landscape ? ControlLandscapeLayout.barTop + ControlLandscapeLayout.barRow + 4 : 58)
                    .padding(.leading, landscape ? ControlLandscapeLayout.side : 12)
                    .transition(reduceMotion ? .identity : .scale(scale: 0.9, anchor: .topLeading).combined(with: .opacity))
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }

    private func close() {
        withAnimation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.26)) { model.adjustOpen = false }
    }
}

/// 调节 (§2.1, user 2026-11-12). UIMenu cannot hold the 指针速度 slider or switches, so this first
/// level is drawn to look like the system menu of the running iOS (SystemMenuMetrics). Order:
/// 指针速度 / 键盘自动弹出 / 接收音频 / 共享麦克风 / 键盘布局 › / 单指划动 ›. Portrait: one column.
/// Landscape (too short for one column): two columns side by side — the slider and the switches,
/// then the two choices — with a hairline between them.
/// 键盘布局 / 单指划动 are real system menus (SwiftUI `Menu` + inline `Picker`) in both orientations
/// (user 2026-11-12: 键盘布局 may scroll): tapping a row opens one anchored to it, on top of this
/// menu, with the system checkmark in the theme tint. Picking an option closes it and the row shows
/// the new value; this menu stays.
private struct AdjustMenu: View {
    @ObservedObject var model: ControlModel
    let landscape: Bool
    let onClose: () -> Void

    var body: some View {
        Group {
            if landscape {
                HStack(alignment: .top, spacing: 0) {
                    switchesColumn.frame(width: SystemMenuMetrics.width)
                    MenuSeparatorLine().frame(width: 0.5)
                    choicesColumn.frame(width: SystemMenuMetrics.width)
                }
                .fixedSize(horizontal: false, vertical: true)
            } else {
                VStack(spacing: 0) {
                    switchesColumn
                    MenuSeparator(inset: SystemMenuMetrics.glass ? SystemMenuMetrics.inset : 0)
                    choicesColumn
                }
                .frame(width: SystemMenuMetrics.width)
            }
        }
        .padding(.vertical, SystemMenuMetrics.verticalPadding)
        .systemMenuSurface()
        .accessibilityElement(children: .contain)
        .accessibilityAddTraits(.isModal)
        .accessibilityAction(.escape) { onClose() }
    }

    /// 指针速度, then the three switches between two inset lines (the system menu's group lines).
    private var switchesColumn: some View {
        VStack(spacing: 0) {
            SpeedRow(model: model)
            MenuSeparator(inset: SystemMenuMetrics.glass ? SystemMenuMetrics.inset : 0)
            SwitchRow(name: "键盘自动弹出", isOn: $model.keyboardAutoPopup)
            if !SystemMenuMetrics.glass { MenuSeparator(inset: 0) }
            AudioSwitchRow(microphone: false)
            if !SystemMenuMetrics.glass { MenuSeparator(inset: 0) }
            AudioSwitchRow(microphone: true)
        }
    }

    private var choicesColumn: some View {
        VStack(spacing: 0) {
            OptionRow(name: "键盘布局", options: ControlKeyboardLayout.allCases, label: \.label, selection: $model.keyboardLayout)
            if !SystemMenuMetrics.glass { MenuSeparator(inset: 0) }
            OptionRow(name: "单指划动", options: OneFingerSwipe.allCases, label: \.label, selection: $model.oneFingerSwipe)
        }
    }
}

/// The vertical hairline between the two landscape columns.
private struct MenuSeparatorLine: View {
    @Environment(\.oink) private var oink
    var body: some View { Rectangle().fill(oink.menuIOSLine) }
}

/// A switch row of 调节 (user 2026-11-12): the name on the left, the system switch (theme tint) on
/// the right, about 44 high; the screen reader reads it as a switch.
private struct SwitchRow: View {
    @Environment(\.oink) private var oink
    let name: String
    @Binding var isOn: Bool

    var body: some View {
        Toggle(isOn: Binding(get: { isOn }, set: { value in
            Haptics.selection()
            isOn = value
        })) {
            Text(name).font(.oink(SystemMenuMetrics.fontSize)).foregroundStyle(oink.ink).lineLimit(1)
        }
        .toggleStyle(.switch)
        .tint(oink.accent)
        .padding(.horizontal, SystemMenuMetrics.inset)
        .padding(.vertical, 6)
        .frame(minHeight: SystemMenuMetrics.rowHeight)
    }
}

/// 接收音频 / 共享麦克风 (user 2026-11-12): this phone's speaker-exit and microphone-sharing
/// settings — the same state and rules as the device page's (CoreSession.localAudio); a failure
/// shows the device page's message.
private struct AudioSwitchRow: View {
    @ObservedObject private var speakerActions = SpeakerActions.shared
    @EnvironmentObject private var core: CoreSession
    @EnvironmentObject private var toasts: ToastCenter
    let microphone: Bool

    var body: some View {
        let disabled = !microphone && speakerActions.forwarding
        let on = !disabled && (microphone ? core.localAudio.microphoneOn : core.localAudio.speakerOn)
        SwitchRow(name: microphone ? "共享麦克风" : "接收音频", isOn: Binding(get: { on }, set: { value in
            Task { @MainActor in
                do {
                    if microphone { try await core.setLocalMicrophone(value) } else { try await core.setLocalSpeaker(value) }
                } catch { toasts.show(error.oinkMessage) }
            }
        }))
        .disabled(disabled)
        .opacity(disabled ? 0.38 : 1)
        .accessibilityIdentifier(microphone ? "audio-adjust-microphone" : "audio-adjust-speaker")
    }
}

/// The system context menu (UIMenu) that the 调节 menu copies, as measured next to the app's own
/// lock / focus menus on the simulator: 250 wide, 17 pt text, rows of about 44; iOS 26 draws it
/// on Liquid Glass with a 22 corner radius, 8 above and below the rows, a 20 text inset, rounded
/// press highlights and no line between rows; earlier versions use the blurred menu material,
/// radius 14, full-width rows and highlights, and a 0.5 line between every row.
enum SystemMenuMetrics {
    static let width: CGFloat = 250
    static let rowHeight: CGFloat = 44
    static let fontSize: CGFloat = 17
    static let valueSize: CGFloat = 16
    static var glass: Bool {
        if #available(iOS 26.0, *) { true } else { false }
    }
    static var cornerRadius: CGFloat { glass ? 22 : 14 }
    static var inset: CGFloat { glass ? 20 : 16 }
    static var verticalPadding: CGFloat { glass ? 8 : 0 }
}

/// Press highlight of a 调节 row, as in the system menu of the running iOS.
private struct SystemMenuRowStyle: ButtonStyle {
    @Environment(\.oink) private var oink
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .contentShape(Rectangle())
            .background {
                if configuration.isPressed {
                    if SystemMenuMetrics.glass {
                        RoundedRectangle(cornerRadius: 14, style: .continuous).fill(oink.press).padding(.horizontal, 8)
                    } else {
                        Rectangle().fill(oink.press)
                    }
                }
            }
    }
}

/// The system menu's background: Liquid Glass on iOS 26, the blurred menu material before.
private struct SystemMenuSurface: ViewModifier {
    @Environment(\.oink) private var oink
    func body(content: Content) -> some View {
        let shape = RoundedRectangle(cornerRadius: SystemMenuMetrics.cornerRadius, style: .continuous)
        if #available(iOS 26.0, *) {
            content
                .clipShape(shape)
                .glassEffect(.regular, in: shape)
                .shadow(color: .black.opacity(oink.dark ? 0.32 : 0.12), radius: 24, y: 10)
        } else {
            content
                .background { shape.fill(.ultraThinMaterial).overlay(shape.fill(oink.menuIOS)) }
                .clipShape(shape)
                .shadow(color: oink.shadow, radius: 20, y: 12)
        }
    }
}

private extension View {
    func systemMenuSurface() -> some View { modifier(SystemMenuSurface()) }
}

private struct MenuSeparator: View {
    @Environment(\.oink) private var oink
    let inset: CGFloat
    var body: some View { Rectangle().fill(oink.menuIOSLine).frame(height: 0.5).padding(.horizontal, inset) }
}

/// One option row of the 调节 menu: name left, current value right in the theme colour; the
/// row is the label of a system menu listing the options.
private struct OptionRow<T: Hashable>: View {
    @Environment(\.oink) private var oink
    let name: String
    let options: [T]
    let label: KeyPath<T, String>
    @Binding var selection: T

    var body: some View {
        let value = selection[keyPath: label]
        Menu {
            Picker(name, selection: Binding(get: { selection }, set: { picked in
                Haptics.selection()
                selection = picked
            })) {
                ForEach(options, id: \.self) { Text($0[keyPath: label]).tag($0) }
            }
            .pickerStyle(.inline)
        } label: {
            HStack(spacing: 10) {
                Text(name).font(.oink(SystemMenuMetrics.fontSize)).foregroundStyle(oink.ink).lineLimit(1).layoutPriority(1)
                Spacer(minLength: 0)
                Text(value).font(.oink(SystemMenuMetrics.valueSize, .medium)).foregroundStyle(oink.accentInk)
                    .lineLimit(1).truncationMode(.tail)
            }
            .padding(.horizontal, SystemMenuMetrics.inset)
            .frame(minHeight: SystemMenuMetrics.rowHeight)
            .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .menuOrder(.fixed)
        .buttonStyle(SystemMenuRowStyle())
        .simultaneousGesture(TapGesture().onEnded { Haptics.selection() })
        .accessibilityLabel(name)
        .accessibilityValue(value)
    }
}

/// 指针速度 row (用户 2026-11-11): the name and the number, the slider always under them
/// (track 4, white thumb 24, 慢 / 快 at the ends). Touching or dragging along the slider sets
/// the value, with a light tick per step.
private struct SpeedRow: View {
    @Environment(\.oink) private var oink
    @ObservedObject var model: ControlModel

    var body: some View {
        let fraction = CGFloat(model.pointerSpeed - 1) / 9
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                Text("指针速度").font(.oink(SystemMenuMetrics.fontSize)).foregroundStyle(oink.ink).lineLimit(1)
                Spacer(minLength: 0)
                Text("\(model.pointerSpeed)").font(.oink(SystemMenuMetrics.fontSize, .semibold)).monospacedDigit()
                    .foregroundStyle(oink.accentInk)
                    .frame(minWidth: 20, alignment: .trailing)
            }
            .frame(minHeight: 24)
            // Track 12 in from the row padding on each side; the 36-high strip around it, and 12
            // more on each end, takes the touches.
            GeometryReader { geo in
                let width = max(1, geo.size.width)
                ZStack(alignment: .leading) {
                    Capsule().fill(oink.track).frame(height: 4)
                    Capsule().fill(oink.accent).frame(width: width * fraction, height: 4)
                    Circle().fill(oink.thumb)
                        .frame(width: 24, height: 24)
                        .shadow(color: oink.shadow, radius: 3, y: 2)
                        .overlay(Circle().stroke(oink.border, lineWidth: 0.5))
                        .offset(x: width * fraction - 12)
                }
                .frame(width: width, height: 36)
                .contentShape(Rectangle().inset(by: -12))
                .gesture(
                    DragGesture(minimumDistance: 0, coordinateSpace: .local)
                        .onChanged { value in
                            let x = min(max(0, value.location.x / width), 1)
                            let speed = Int((1 + x * 9).rounded())
                            if speed != model.pointerSpeed {
                                Haptics.selection()
                                model.pointerSpeed = speed
                            }
                        }
                )
            }
            .frame(height: 36)
            .padding(.horizontal, 12)
            HStack {
                Text("慢"); Spacer(); Text("快")
            }
            .font(.oink(11))
            .foregroundStyle(oink.faint)
            .padding(.horizontal, 4)
        }
        .padding(.horizontal, SystemMenuMetrics.inset)
        .padding(.top, 12)
        .padding(.bottom, 8)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("指针速度")
        .accessibilityValue("\(model.pointerSpeed)")
        .accessibilityAdjustableAction { direction in
            switch direction {
            case .increment: model.pointerSpeed = min(10, model.pointerSpeed + 1)
            case .decrement: model.pointerSpeed = max(1, model.pointerSpeed - 1)
            @unknown default: break
            }
        }
    }
}
