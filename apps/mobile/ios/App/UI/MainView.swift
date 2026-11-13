import SwiftUI
import UIKit

enum HomeTab: String, Hashable { case devices, transfer, control }

/// App root: the system tab bar (devices / transfer / control), the settings layer that
/// covers the whole app, and the sheets that sit above the tabs.
struct MainView: View {
    @ObservedObject private var speakerActions = SpeakerActions.shared
    @ObservedObject private var forwardLaunch = ForwardLaunchRequest.shared
    @Environment(\.scenePhase) private var scenePhase
    @EnvironmentObject private var core: CoreSession
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var tab: HomeTab
    /// The control page's state, held here (not observed: MainView does not redraw for it).
    @State private var control = ControlModel()
    @State private var addPresented: Bool
    @State private var settingsPresented: Bool
    @StateObject private var pairing = PairingFlow()
    @StateObject private var toasts = ToastCenter()

    init() {
        let scene = DesignScene.current
        let initialTab: HomeTab
        if let scene {
            initialTab = scene.hasPrefix("control") ? .control
                : (scene.hasPrefix("transfer") || scene.hasPrefix("share")) ? .transfer : .devices
        } else {
            initialTab = HomeTab(rawValue: UserDefaults.standard.string(forKey: OinkPrefs.homeTab) ?? "") ?? .devices
        }
        _tab = State(initialValue: initialTab)
        let add = scene.map { $0.hasPrefix("add") || $0.hasPrefix("manual") || $0.hasPrefix("pair") } ?? false
        _addPresented = State(initialValue: add)
        let settings = scene.map { $0.hasPrefix("settings") || $0 == "shortcut-guide" || $0 == "licenses" } ?? false
        _settingsPresented = State(initialValue: settings)
    }

    var body: some View {
        ZStack {
            TabView(selection: $tab) {
                Tab(value: HomeTab.devices) {
                    DevicesTab(addPresented: $addPresented, onSettings: openSettings)
                } label: { tabLabel("设备", icon: "Lucide-monitor", .devices) }
                Tab(value: HomeTab.transfer) {
                    TransferView()
                } label: { tabLabel("传输", icon: "Lucide-arrow-down-up", .transfer) }
                Tab(value: HomeTab.control) {
                    // The control page is drawn over the tab view (below), not in it. Under it the
                    // system tab bar is hidden too: covered, it still answered the screen reader and
                    // UI tests with hittable 设备 / 传输 (integration 400005 round 4).
                    oink.bg.ignoresSafeArea()
                        .toolbar(.hidden, for: .tabBar)
                } label: { tabLabel("控制", icon: "Tab-control", .control) }
            }
            // Under the control page: out of the screen reader's way, and its tab bar never takes a
            // touch meant for the page.
            .accessibilityHidden(tab == .control)
            .allowsHitTesting(tab != .control)
            .onChange(of: tab) { old, value in
                if value != .devices { addPresented = false }
                // The outgoing page stays drawn for its bar transition. Release input at the
                // selection change, rather than waiting for that view's disappearance.
                if old == .control && value != .control {
                    control.deactivate()
                    control.leftPage()
                }
            }

            // The control page covers the whole tab view, system tab bar included, and draws its own
            // tabs (用户 2026-11-12: in portrait the bar moved left with the keyboard button beside it;
            // in landscape in the top bar). Inside the TabView it could not work (integration 400004
            // / 400005): the system tab bar's container (402 × 83 at the bottom) stays above the
            // tab's content even with `.toolbar(.hidden, for: .tabBar)` — it took the taps on the
            // page's tab bar and keyboard button, and on iOS 26 the system bar floated over the
            // keyboard — and the TabView rebuilt the tab's content after hiding its bar, so state
            // set in `onAppear` was lost (the page's bar stayed centred with its button invisible;
            // the control-adjust scene lost its open menu). Out here neither can happen.
            if tab == .control {
                ControlTab(model: control, isActive: true, onSelectTab: { tab = $0 })
                    .transition(.modifier(active: ControlArrival(progress: 0), identity: ControlArrival(progress: 1)))
                    .zIndex(5)
            }

            if settingsPresented {
                SettingsView(onClose: { withAnimation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.32)) { settingsPresented = false } },
                             onLeftGroup: {
                                 withAnimation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.32)) { settingsPresented = false }
                                 addPresented = false
                                 tab = .devices
                                 toasts.show("已退出设备组")
                             })
                    .transition(.move(edge: .leading))
                    .zIndex(10)
            }
            #if DEBUG
            if let share = ShareScene.state {
                ZStack {
                    ShareDemoHost()
                    ShareCardView(state: share, freezeT: ShareScene.freezeHandoff == nil ? 0.30 : nil, freezeHandoff: ShareScene.freezeHandoff) {
                        if share.phase != .succeeded { Button(AppText.pick("取消", "Cancel")) {}.buttonStyle(ShareCapsuleButtonStyle()) }
                    }
                }
                    .zIndex(30)
            }
            if let scene = DesignScene.current, scene.hasPrefix("share-pick") {
                SharePickDemo(scene: scene).zIndex(30)
            }
            #endif
            if let toast = toasts.toast {
                VStack { ToastView(toast: toast) { toasts.act() }.padding(.top, 72); Spacer() }
                    .zIndex(20)
            }
        }
        .onChange(of: speakerActions.started) { _, _ in toasts.show("调节本机音量不影响转发") }
        .onChange(of: speakerActions.errorRevision) { _, _ in if let error = speakerActions.error { toasts.show(error) } }
        .onChange(of: forwardLaunch.pending) { _, _ in forwardLaunch.presentIfReady() }
        .onChange(of: scenePhase) { _, phase in if phase == .active { forwardLaunch.presentIfReady() } }
        .onAppear { forwardLaunch.presentIfReady() }
        .animation(reduceMotion ? nil : .timingCurve(0.22, 1, 0.36, 1, duration: 0.32), value: tab)
        .environmentObject(pairing)
        .environmentObject(toasts)
        .sheet(item: $pairing.sheet) { sheet in
            PairingSheetHost(sheet: sheet)
                .environmentObject(core)
                .environmentObject(pairing)
                .environmentObject(toasts)
                .oinkThemed()
        }
        // M20 (D8): the restored group was not this phone's; a new one replaced it.
        .alert("请重新配对", isPresented: $core.groupReplaced) {
            Button("好") {}
        } message: {
            Text("保存的设备组不包含本机身份（换用新 iPhone 或从备份恢复后，本机身份不会随之迁移），已为本机新建设备组。请在电脑上移除旧的本机，再重新添加。")
        }
        // A completed pairing returns to the devices page from wherever the app is.
        .onChange(of: pairing.pairedReturns) { _, _ in
            settingsPresented = false
            addPresented = false
            tab = .devices
        }
        .onChange(of: core.pairingCompleted) { _, completed in
            guard let completed else { return }
            pairing.pairingCompleted(completed)
            core.pairingCompleted = nil
        }
        .task {
            pairing.attach(core: core, toasts: toasts)
            #if DEBUG
            pairing.applyDesignScene()
            #endif
        }
    }

    private func openSettings() {
        withAnimation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.32)) { settingsPresented = true }
    }

    /// Tab icon: stroke 2.2 when selected, 1.8 otherwise (mockup `m-tabs`).
    private func tabLabel(_ title: String, icon: String, _ value: HomeTab) -> some View {
        Label {
            Text(title)
        } icon: {
            Image("\(icon)-\(tab == value ? "2_2" : "1_8")").renderingMode(.template)
        }
    }
}

/// The 64 pt toolbar at the top of each page (`.m-topbar`).
struct TopBar<Leading: View, Trailing: View>: View {
    @Environment(\.oink) private var oink
    var title: String?
    @ViewBuilder var leading: () -> Leading
    @ViewBuilder var trailing: () -> Trailing
    var body: some View {
        ZStack {
            HStack(spacing: 8) {
                leading()
                Spacer(minLength: 0)
                trailing()
            }
            .padding(.horizontal, 16)
            if let title {
                Text(title).font(.oink(18, .semibold)).foregroundStyle(oink.ink).allowsHitTesting(false)
            }
        }
        .frame(height: 64)
        .background(oink.bg)
    }
}

/// 48 × 48 toolbar icon button.
struct ToolbarIconButton<Icon: View>: View {
    let label: String
    let action: () -> Void
    @ViewBuilder var icon: () -> Icon
    var body: some View {
        Button(action: action) {
            icon().frame(width: 48, height: 48).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(label)
    }
}

struct SettingsGearButton: View {
    @Environment(\.oink) private var oink
    let action: () -> Void
    var body: some View {
        ToolbarIconButton(label: "打开设置", action: action) { Lucide(name: "settings", size: 23, color: oink.ink) }
    }
}

/// UINavigationController keeps its edge-swipe back gesture while the bar is hidden.
extension UINavigationController: @retroactive UIGestureRecognizerDelegate {
    override open func viewDidLoad() {
        super.viewDidLoad()
        interactivePopGestureRecognizer?.delegate = self
    }
    public func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        viewControllers.count > 1
    }
}

#if DEBUG
/// Share-card design states (frozen at t = 0.30 like the mockup screenshots).
enum ShareScene {
    static var state: ShareUiState? {
        switch DesignScene.current {
        case "share-indeterminate": ShareUiState(phase: .sending, statusText: "正在发送…", progress: nil)
        case "share-sending": ShareUiState(phase: .sending, statusText: "正在发送… \(ShareStatusFormatter.rate(bytesPerSecond: 2_470_000))", progress: 0.52)
        case "share-sending-slow": ShareUiState(phase: .sending, statusText: "正在发送… \(ShareStatusFormatter.rate(bytesPerSecond: 512))", progress: 0.18)
        case "share-done": ShareUiState(phase: .succeeded, statusText: "已完成，耗时 \(ShareStatusFormatter.elapsed(seconds: 3.18)) 秒", progress: 1)
        // Waiting for the only other device, a computer or a phone, and the hand-over (share-anim SPEC §2.1, §5.9).
        case "share-wait": ShareUiState(phase: .waiting, statusText: ShareUiState.waitingText, progress: nil)
        case "share-wait-phone": ShareUiState(phase: .waiting, statusText: ShareUiState.waitingText, progress: nil, toPhone: true)
        case "share-wait-online": ShareUiState(phase: .sending, statusText: "正在发送…", progress: nil)
        case "share-phone-sending": ShareUiState(phase: .sending, statusText: "正在发送给 Android… \(ShareStatusFormatter.rate(bytesPerSecond: 2_470_000))",
                                                 progress: 0.52, toPhone: true)
        case "share-phone-done": ShareUiState(phase: .succeeded, statusText: "已发送给 Android，耗时 \(ShareStatusFormatter.elapsed(seconds: 6.42)) 秒",
                                              progress: 1, toPhone: true)
        default: nil
        }
    }
    /// `share-wait-online`: 230 ms into the hand-over.
    static var freezeHandoff: CGFloat? { DesignScene.is("share-wait-online") ? 230 : nil }
}
#endif

private struct ControlBarArrivalKey: EnvironmentKey { static let defaultValue: CGFloat = 1 }
extension EnvironmentValues {
    var controlBarArrival: CGFloat {
        get { self[ControlBarArrivalKey.self] }
        set { self[ControlBarArrivalKey.self] = newValue }
    }
}
/// The tabs move from/to the system bar's centre while the keyboard button grows/fades beside
/// them. The surrounding page fades with the same transaction in both directions.
private struct ControlArrival: ViewModifier, Animatable {
    var progress: CGFloat
    var animatableData: CGFloat { get { progress } set { progress = newValue } }
    func body(content: Content) -> some View {
        content.environment(\.controlBarArrival, progress).opacity(progress)
    }
}
