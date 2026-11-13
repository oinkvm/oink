import SwiftUI
import UIKit

/// Settings (mockup `SettingsPage.jsx`, iOS variant): a layer over the whole app with its own
/// push stack — option pickers, the one-tap copy guide and the open-source licenses.
struct SettingsView: View {
    enum Route: Hashable { case picker(PickerKey), deviceName, shortcut, licenses, license(String) }

    @EnvironmentObject private var core: CoreSession
    @Environment(\.oink) private var oink
    let onClose: () -> Void
    /// This phone left its device group: the app shows the devices page and the toast.
    let onLeftGroup: () -> Void
    @State private var path: [Route]
    @State private var dismissDrag: CGFloat = 0
    @State private var confirmClear = false
    @State private var confirmLeave = false
    @State private var clearing = false
    /// Bytes 清理缓存 frees, from the core; nil while the session is not running.
    @State private var cacheBytes: Int64?
    @StateObject private var toasts = ToastCenter()
    @StateObject private var history = TransferHistoryClear(hasHistory: { CoreSession.shared.hasTransferHistory() },
                                                            clear: { try CoreSession.shared.clearTransferHistory() })

    init(onClose: @escaping () -> Void, onLeftGroup: @escaping () -> Void) {
        self.onClose = onClose
        self.onLeftGroup = onLeftGroup
        var initial: [Route] = []
        switch DesignScene.current {
        case "shortcut-guide": initial = [.shortcut]
        case "licenses": initial = [.licenses]
        case "settings-device-name": initial = [.deviceName]
        case let scene? where scene.hasPrefix("settings-picker-"):
            let key: [String: PickerKey] = ["scheme": .scheme, "language": .language, "home": .homeTab, "mic": .micAutoOff]
            if let k = key[String(scene.dropFirst("settings-picker-".count))] { initial = [.picker(k)] }
        default: break
        }
        _path = State(initialValue: initial)
    }

    var body: some View {
        NavigationStack(path: $path) {
            SettingsPage(title: "设置", back: onClose, backLabel: "关闭设置") {
                MainSettings(cacheBytes: cacheBytes, clearing: clearing, historyDisabled: history.disabled,
                             historyBusy: history.clearing, push: { path.append($0) },
                             clearHistory: history.request, clearCache: { confirmClear = true },
                             leaveGroup: { confirmLeave = true })
            }
            .navigationDestination(for: Route.self) { route in
                switch route {
                case .picker(let key):
                    SettingsPage(title: key.title, back: pop) { PickerPage(key: key) }
                case .deviceName:
                    SettingsPage(title: "设备名", back: pop) { DeviceNamePage() }
                case .shortcut:
                    SettingsPage(title: "一键复制", back: pop) { ShortcutGuide() }
                case .licenses:
                    SettingsPage(title: "开源许可", back: pop) { LicensesPage(push: { path.append($0) }) }
                case .license(let id):
                    SettingsPage(title: OpenSourceLicenses.find(id)?.name ?? id, back: pop) { LicenseText(id: id) }
                }
            }
        }
        .environmentObject(toasts)
        .overlay(alignment: .top) {
            if let toast = toasts.toast { ToastView(toast: toast) { toasts.act() }.padding(.top, 72) }
        }
        .alert("清理缓存？", isPresented: $confirmClear) {
            Button("取消", role: .cancel) {}
            Button("清理", role: .destructive, action: clearCache)
        } message: { Text("将删除已下载的图片和文件，共 \(formatBytes(cacheBytes ?? 0))。消息记录会保留。") }
        .alert("退出设备组", isPresented: $confirmLeave) {
            Button("取消", role: .cancel) {}
            Button("退出", role: .destructive, action: leaveGroup)
        } message: { Text("退出后，需要重新配对才能和组内设备共享。") }
        .alert("清理传输记录？", isPresented: $history.confirming) {
            Button("取消", role: .cancel) {}
            Button("清理", role: .destructive) {
                let text = history.confirm()
                refreshCacheBytes()
                toasts.show(text)
            }
        } message: { Text("将删除本机的所有传输记录，以及已下载的图片和文件。其它设备上的记录不受影响。") }
        .onAppear {
            #if DEBUG
            if DesignScene.current != nil {
                cacheBytes = 1_352_663_040
                switch DesignScene.current {
                case "settings-clear-cache": confirmClear = true
                case "settings-cache-cleared": cacheBytes = 0; toasts.show("已清理 1.26 GB", duration: 3600)
                default: break
                }
                return
            }
            #endif
            refreshCacheBytes()
            history.refresh()
        }
        .onChange(of: core.sessionRevision) { _, _ in refreshCacheBytes(); history.refresh() }
        // Records arriving or ending while Settings is open.
        .onChange(of: core.transferChange) { _, _ in history.refresh() }
        .offset(x: dismissDrag)
        .gesture(SettingsBackGesture(enabled: path.isEmpty, moved: { x in
            var transaction = Transaction()
            transaction.disablesAnimations = true
            withTransaction(transaction) { dismissDrag = x }
        }, ended: { distance, width in
            if width > 0 && distance > width / 3 { onClose() } else { cancelDismiss() }
        }, cancelled: cancelDismiss))
    }

    private func cancelDismiss() {
        withAnimation(.easeOut(duration: 0.22)) { dismissDrag = 0 }
    }

    private func pop() { if !path.isEmpty { path.removeLast() } }

    private func leaveGroup() {
        Task { @MainActor in
            do {
                try await core.leaveCurrentGroup()
                onLeftGroup()
            } catch {
                toasts.show(error.oinkMessage)
            }
        }
    }

    private func refreshCacheBytes() { cacheBytes = try? core.transferCacheBytes() }

    private func clearCache() {
        let size = cacheBytes ?? 0
        clearing = true
        Task { @MainActor in
            do {
                try core.clearTransferCache()
                refreshCacheBytes()
                clearing = false
                toasts.show("已清理 \(formatBytes(size))")
            } catch {
                clearing = false
                toasts.show(error.oinkMessage)
            }
        }
    }
}

/// A native edge pan cancels the row's tap only after recognizing a drag. A simultaneous SwiftUI
/// drag also activated the row on release (iOS 27 simulator); an overlay touch strip would block
/// ordinary edge taps. The recognizer is disabled on child pages, retaining their native back.
private struct SettingsBackGesture: UIGestureRecognizerRepresentable {
    let enabled: Bool
    let moved: (CGFloat) -> Void
    let ended: (CGFloat, CGFloat) -> Void
    let cancelled: () -> Void

    func makeUIGestureRecognizer(context: Context) -> UIScreenEdgePanGestureRecognizer {
        let recognizer = UIScreenEdgePanGestureRecognizer()
        recognizer.edges = .right
        recognizer.isEnabled = enabled
        return recognizer
    }

    func updateUIGestureRecognizer(_ recognizer: UIScreenEdgePanGestureRecognizer, context: Context) {
        recognizer.isEnabled = enabled
    }

    func handleUIGestureRecognizerAction(_ recognizer: UIScreenEdgePanGestureRecognizer, context: Context) {
        // Window coordinates stay stable while the settings layer follows the finger.
        let x = recognizer.translation(in: recognizer.view?.window).x
        switch recognizer.state {
        case .began, .changed: moved(min(0, x))
        case .ended: ended(-x, recognizer.view?.bounds.width ?? 0)
        case .cancelled, .failed: cancelled()
        default: break
        }
    }
}

/// Page frame: 64 pt header with a back chevron and a centred title, scrolling content.
private struct SettingsPage<Content: View>: View {
    @Environment(\.oink) private var oink
    let title: String
    let back: () -> Void
    var backLabel = "返回"
    @ViewBuilder var content: () -> Content
    var body: some View {
        VStack(spacing: 0) {
            TopBar(title: title) {
                ToolbarIconButton(label: backLabel, action: back) { Lucide(name: "chevron-left", size: 23, color: oink.ink) }
            } trailing: { Color.clear.frame(width: 48, height: 48) }
            ScrollViewReader { reader in
                ScrollView {
                    VStack(alignment: .leading, spacing: 24) { content() }
                        .frame(maxWidth: 620)
                        .frame(maxWidth: .infinity)
                        .padding(.horizontal, 16)
                        .padding(.top, 12)
                        .padding(.bottom, 40)
                }
                .scrollIndicators(.hidden)
                .task {
                    #if DEBUG
                    let target: String? = switch DesignScene.current {
                    case "settings-transfer", "settings-clear-cache", "settings-cache-cleared": "transfer"
                    case "settings-audio": "audio"
                    case "settings-end": "end"
                    default: nil
                    }
                    if let target, title == "设置" {
                        try? await Task.sleep(for: .milliseconds(150))
                        reader.scrollTo(target, anchor: target == "end" ? .bottom : .top)
                    }
                    #endif
                }
            }
        }
        .background(oink.bg.ignoresSafeArea())
        .toolbar(.hidden, for: .navigationBar)
    }
}

/// Group card with an outside title and an optional footer (`.sp-section`).
private struct SettingsSection<Content: View>: View {
    @Environment(\.oink) private var oink
    var title: String?
    var footer: String?
    @ViewBuilder var content: () -> Content
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let title {
                Text(title).font(.oink(13, .semibold)).kerning(0.4).foregroundStyle(oink.muted)
                    .padding(.leading, 16).padding(.bottom, 8)
            }
            VStack(spacing: 0) {
                Group(subviews: content()) { rows in
                    ForEach(Array(rows.enumerated()), id: \.offset) { index, row in
                        row.overlay(alignment: .top) { if index > 0 { Hairline().padding(.leading, 16) } }
                    }
                }
            }
            .background(oink.card, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
            .clipShape(RoundedRectangle(cornerRadius: 20, style: .continuous))
            if let footer {
                Text(footer).font(.oink(12.5)).foregroundStyle(oink.muted).lineSpacing(12.5 * 0.55 - 3)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 16).padding(.top, 8)
            }
        }
    }
}

/// Value row: title, trailing grey value and chevron (iOS).
private struct ValueRow: View {
    @Environment(\.oink) private var oink
    let title: String
    var value: String?
    var sub: String?
    var busy = false
    var chevron = true
    var disabled = false
    var action: (() -> Void)?
    var body: some View {
        let row = HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.oink(16)).foregroundStyle(disabled ? oink.faint : oink.ink).frame(minHeight: 22.4)
                if let sub { Text(sub).font(.oink(13)).foregroundStyle(oink.muted).frame(minHeight: 18.85) }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            if busy { OinkSpinner(size: 18) }
            if let value { Text(value).font(.oink(15).monospacedDigit()).foregroundStyle(oink.muted).lineLimit(1) }
            if chevron { Lucide(name: "chevron-right", size: 18, color: oink.faint).padding(.trailing, -4) }
        }
        .padding(.horizontal, 16).padding(.vertical, 12)
        .frame(minHeight: 54)
        if let action {
            Button { Haptics.selection(); action() } label: { row }
                .buttonStyle(RowButtonStyle()).disabled(disabled)
                .accessibilityLabel(value.map { "\(title)，\($0)" } ?? title)
        } else { row }
    }
}

private struct SwitchRow: View {
    @Environment(\.oink) private var oink
    let title: String
    var sub: String?
    @Binding var isOn: Bool
    var body: some View {
        Button { Haptics.selection(); isOn.toggle() } label: {
            HStack(spacing: 12) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(title).font(.oink(16)).foregroundStyle(oink.ink)
                    if let sub { Text(sub).font(.oink(13)).foregroundStyle(oink.muted).fixedSize(horizontal: false, vertical: true) }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                OinkSwitch(isOn: isOn)
            }
            .padding(.horizontal, 16).padding(.vertical, 12)
            .frame(minHeight: 54)
        }
        .buttonStyle(RowButtonStyle())
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isToggle)
        .accessibilityValue(isOn ? "开" : "关")
    }
}

// MARK: - Main page

enum PickerKey: String, Hashable, CaseIterable {
    case scheme, language, homeTab, micAutoOff, speakerAutoOff, speakerTap
    var title: String {
        switch self {
        case .scheme: "深浅色"; case .language: "语言"; case .homeTab: "默认页面"
        case .micAutoOff: "麦克风自动关闭"; case .speakerAutoOff: "扬声器自动关闭"
        case .speakerTap: "单击扬声器按钮行为"
        }
    }
    var storageKey: String {
        switch self {
        case .scheme: OinkPrefs.scheme; case .language: OinkPrefs.language; case .homeTab: OinkPrefs.homeTab
        case .micAutoOff: OinkPrefs.micAutoOff; case .speakerAutoOff: OinkPrefs.speakerAutoOff
        case .speakerTap: SpeakerActions.preferenceKey
        }
    }
    var defaultValue: String {
        switch self { case .scheme, .language: "system"; case .homeTab: "devices"; case .micAutoOff, .speakerAutoOff: "5"; case .speakerTap: "exit" }
    }
    var options: [(String, String)] {
        let timeouts = [("1", "1 分钟"), ("5", "5 分钟"), ("10", "10 分钟"), ("20", "20 分钟"), ("30", "30 分钟"), ("60", "60 分钟"), ("0", "无限制")]
        switch self {
        case .scheme: return [("system", "跟随系统"), ("light", "浅色"), ("dark", "深色")]
        case .language: return [("system", "跟随系统"), ("zh-Hans", "简体中文"), ("en", "English"), ("ja", "日本語"), ("ko", "한국어")]
        case .homeTab: return [("devices", "设备"), ("transfer", "传输"), ("control", "控制")]
        case .micAutoOff, .speakerAutoOff: return timeouts
        case .speakerTap: return [("exit", "设为出口"), ("forward", "转发音频")]
        }
    }
    var footer: String? {
        switch self {
        case .homeTab: "下次打开 Oink 时先显示这个页面。"
        case .micAutoOff: "本机作为麦克风时，如果这段时间里一直没有设备在用，就自动关闭。刚开启时从开启那一刻开始计时。"
        case .speakerAutoOff: "本机作为扬声器时，如果这段时间里一直没有收到声音，就自动关闭。刚开启时从开启那一刻开始计时。"
        default: nil
        }
    }
    /// The audio auto-close times live in the Rust core; the others are app
    /// preferences.
    var isCoreSetting: Bool { self == .micAutoOff || self == .speakerAutoOff }

    /// Current value as text; nil when a core setting is not readable yet.
    @MainActor func current(_ core: CoreSession) -> String? {
        switch self {
        case .micAutoOff: return core.settings.map { String($0.microphoneIdleTimeout.rawValue) }
        case .speakerAutoOff: return core.settings.map { String($0.speakerIdleTimeout.rawValue) }
        case .scheme, .language, .homeTab, .speakerTap: return UserDefaults.standard.string(forKey: storageKey) ?? defaultValue
        }
    }

    /// Stores a choice. Core settings are written to the core (and read back);
    /// a refusal throws its reason.
    @MainActor func set(_ value: String, core: CoreSession) throws {
        switch self {
        case .micAutoOff, .speakerAutoOff:
            guard let timeout = AudioIdleTimeout(rawValue: Int(value) ?? -1) else { return }
            try core.setIdleTimeout(self == .micAutoOff ? .microphone : .speaker, timeout)
        case .language:
            UserDefaults.standard.set(value, forKey: storageKey)
            // The app's preferred language (system pickers, menus and any
            // localized resource) follows the choice from the next launch.
            if value == "system" { UserDefaults.standard.removeObject(forKey: "AppleLanguages") }
            else { UserDefaults.standard.set([value], forKey: "AppleLanguages") }
        case .scheme, .homeTab, .speakerTap:
            UserDefaults.standard.set(value, forKey: storageKey)
        }
    }
    func label(_ value: String?) -> String {
        guard let value else { return "—" }
        return options.first { $0.0 == value }?.1 ?? value
    }
}

private struct MainSettings: View {
    @EnvironmentObject private var core: CoreSession
    @EnvironmentObject private var toasts: ToastCenter
    @Environment(\.oink) private var oink
    @AppStorage(OinkPrefs.accent, store: OinkPrefs.shared) private var accent = "pink"
    @AppStorage(OinkPrefs.scheme) private var scheme = "system"
    @AppStorage(OinkPrefs.language) private var language = "system"
    @AppStorage(OinkPrefs.homeTab) private var homeTab = "devices"
    @AppStorage(SpeakerActions.preferenceKey) private var speakerTap = "exit"
    let cacheBytes: Int64?
    let clearing: Bool
    let historyDisabled: Bool
    let historyBusy: Bool
    let push: (SettingsView.Route) -> Void
    let clearHistory: () -> Void
    let clearCache: () -> Void
    let leaveGroup: () -> Void

    /// 退出设备组 only while the group has a device besides this phone (user 2026-11-11).
    private var canLeave: Bool { core.cards?.cards.contains { !$0.isSelf } == true }

    /// Echo cancellation is a core setting: the switch shows the core's value
    /// and writes through to it.
    private var echoCancel: Binding<Bool> {
        Binding(get: { core.settings?.echoCancellation ?? false }, set: { value in
            do { try core.setEchoCancellation(value) } catch { toasts.show(error.oinkMessage) }
        })
    }

    var body: some View {
        SettingsSection(title: "本机") {
            ValueRow(title: "设备名", value: core.localDeviceName?.name ?? "—") { push(.deviceName) }
        }
        SettingsSection(title: "外观") {
            AccentRow(selection: $accent)
            ValueRow(title: "深浅色", value: PickerKey.scheme.label(scheme)) { push(.picker(.scheme)) }
            ValueRow(title: "语言", value: PickerKey.language.label(language)) { push(.picker(.language)) }
        }
        .id("appearance")
        SettingsSection(title: "首页") {
            ValueRow(title: "默认页面", value: PickerKey.homeTab.label(homeTab)) { push(.picker(.homeTab)) }
        }
        SettingsSection(title: "传输", footer: "清理缓存会删除已下载的图片和文件，消息记录会保留。") {
            ValueRow(title: "清理传输记录", busy: historyBusy, disabled: historyDisabled, action: clearHistory)
            ValueRow(title: "清理缓存", value: clearing ? "正在清理…" : cacheBytes.map(formatBytes) ?? "—", busy: clearing,
                     disabled: clearing || (cacheBytes ?? 0) <= 0, action: clearCache)
        }
        .id("transfer")
        SettingsSection(title: "剪贴板", footer: "用快捷指令或操作按钮，一键把其它设备最新共享的文字或图片拷贝到本机。") {
            ValueRow(title: "一键复制最新内容", value: "设置方法") { push(.shortcut) }
        }
        SettingsSection(title: "音频", footer: "开启麦克风或扬声器后，如果一直没有设备在用，到时间会自动关闭。") {
            ValueRow(title: "单击扬声器按钮行为", value: PickerKey.speakerTap.label(speakerTap)) { push(.picker(.speakerTap)) }
            ValueRow(title: "麦克风自动关闭", value: PickerKey.micAutoOff.label(PickerKey.micAutoOff.current(core))) { push(.picker(.micAutoOff)) }
            ValueRow(title: "扬声器自动关闭", value: PickerKey.speakerAutoOff.label(PickerKey.speakerAutoOff.current(core))) { push(.picker(.speakerAutoOff)) }
            SwitchRow(title: "回声消除", sub: "消除麦克风录制的本机播放的音频", isOn: echoCancel)
        }
        .id("audio")
        SettingsSection(title: "关于") {
            ValueRow(title: "版本", value: versionText, chevron: false)
            ValueRow(title: "开源许可") { push(.licenses) }
        }
        .id(canLeave ? "about" : "end")
        if canLeave {
            Button("退出设备组") { Haptics.selection(); leaveGroup() }
                .buttonStyle(CapsuleButtonStyle(kind: .danger, fullWidth: true))
                .id("end")
        }
    }

    private var versionText: String {
        let info = Bundle.main.infoDictionary
        let version = info?["CFBundleShortVersionString"] as? String ?? "—"
        let build = info?["CFBundleVersion"] as? String ?? ""
        return build.isEmpty ? version : "\(version) (\(build))"
    }
}

/// Theme colour swatches: 28 colour in a 40 target; selection = 2 pt gap + 2 pt ring + check.
private struct AccentRow: View {
    @Environment(\.oink) private var oink
    @Binding var selection: String
    var body: some View {
        HStack(spacing: 12) {
            Text("主题色").font(.oink(16)).foregroundStyle(oink.ink).frame(maxWidth: .infinity, alignment: .leading)
            HStack(spacing: 2) {
                ForEach(OinkPalette.Accent.allCases) { accent in
                    let selected = selection == accent.rawValue
                    Button { Haptics.selection(); selection = accent.rawValue } label: {
                        ZStack {
                            if selected { Circle().strokeBorder(Color(hex: accent.base), lineWidth: 2).frame(width: 36, height: 36) }
                            Circle().fill(Color(hex: accent.base)).frame(width: 28, height: 28)
                            if selected { Lucide(name: "check", size: 15, stroke: "3", color: oink.onAccent) }
                        }
                        .frame(width: 40, height: 40)
                    }
                    .buttonStyle(ShrinkButtonStyle())
                    .accessibilityLabel(accent.label)
                    .accessibilityAddTraits(selected ? [.isSelected] : [])
                }
            }
            .padding(.trailing, -6)
        }
        .padding(.horizontal, 16).padding(.vertical, 8)
        .frame(minHeight: 56)
    }
}

/// iOS option page: the current option is ticked; choosing stays on the page.
private struct PickerPage: View {
    @EnvironmentObject private var core: CoreSession
    @EnvironmentObject private var toasts: ToastCenter
    @Environment(\.oink) private var oink
    let key: PickerKey
    @State private var stored: String?
    /// Core settings tick what the core reports; app preferences what was stored.
    private var value: String? { key.isCoreSetting ? key.current(core) : stored }
    var body: some View {
        SettingsSection(footer: key.footer) {
            ForEach(key.options, id: \.0) { option, label in
                Button {
                    Haptics.selection()
                    do { try key.set(option, core: core); stored = option }
                    catch { toasts.show(error.oinkMessage) }
                } label: {
                    HStack(spacing: 12) {
                        Text(label).font(.oink(16)).foregroundStyle(oink.ink).frame(maxWidth: .infinity, alignment: .leading)
                        if value == option { Lucide(name: "check", size: 20, stroke: "2_4", color: oink.accentInk) }
                    }
                    .padding(.horizontal, 16).padding(.vertical, 12).frame(minHeight: 54)
                }
                .buttonStyle(RowButtonStyle())
                .accessibilityAddTraits(value == option ? [.isSelected] : [])
            }
        }
        .onAppear { stored = key.current(core) }
    }
}

/// 设备名: the name the other devices see. Saved to the core on return or when
/// leaving the page; 恢复默认 returns to the system name.
private struct DeviceNamePage: View {
    @EnvironmentObject private var core: CoreSession
    @EnvironmentObject private var toasts: ToastCenter
    @Environment(\.oink) private var oink
    @State private var text = ""
    /// Only what the user typed is saved (not the not-yet-loaded empty field).
    @State private var edited = false
    @FocusState private var focused: Bool

    var body: some View {
        let name = core.localDeviceName
        SettingsSection(footer: "其它设备上显示的本机名称。默认名称是「\(name?.systemName ?? "—")」。") {
            TextField("", text: Binding(get: { text }, set: { text = $0; edited = true }),
                      prompt: Text(name?.systemName ?? "").foregroundStyle(oink.hint))
                .font(.oink(16)).foregroundStyle(oink.ink)
                .focused($focused)
                .submitLabel(.done)
                .onSubmit(save)
                .padding(.horizontal, 16).frame(minHeight: 54)
                .task { text = core.localDeviceName?.name ?? ""; focused = true }
                // The name arrives with the session; fill it in if nothing was typed yet.
                .onChange(of: core.localDeviceName?.name) { _, name in if text.isEmpty, let name { text = name } }
        }
        SettingsSection {
            ValueRow(title: "恢复默认名称", chevron: false, disabled: name?.custom != true) {
                apply(nil)
            }
        }
        .onDisappear(perform: save)
    }

    private func save() {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard edited, let current = core.localDeviceName, trimmed != current.name else { return }
        edited = false
        apply(trimmed.isEmpty ? nil : trimmed)
    }

    private func apply(_ value: String?) {
        do {
            try core.setLocalDeviceName(value)
            text = core.localDeviceName?.name ?? text
        } catch {
            toasts.show(error.oinkMessage)
            text = core.localDeviceName?.name ?? text
        }
    }
}

/// 一键复制 guide (iOS, mockup A of 2026-11-12): the two shortcut actions, one tap that adds the
/// bundled 「Oink 复制」 shortcut, where to put it (Control Center, Action button), and building it
/// by hand as the fallback.
private struct ShortcutGuide: View {
    @Environment(\.oink) private var oink
    @EnvironmentObject private var toasts: ToastCenter
    var body: some View {
        VStack(spacing: 6) {
            block(color: oink.accent, icon: "arrow-down-up", stroke: "2_2", "获取 Oink 最新共享内容")
            Lucide(name: "arrow-down", size: 18, color: oink.faint)
            block(color: oink.systemApp, icon: "clipboard-copy", stroke: "2_2", "拷贝到剪贴板")
        }
        .padding(.vertical, 22).padding(.horizontal, 18)
        .frame(maxWidth: .infinity)
        .background(oink.card, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
        .accessibilityHidden(true)
        VStack(spacing: 10) {
            Button(action: addShortcut) {
                HStack(spacing: 6) {
                    Lucide(name: "plus", size: 18, stroke: "2_4", color: oink.accentInk)
                    Text("添加「\(ShortcutFile.shortcutName)」快捷指令")
                }
            }
            .buttonStyle(CapsuleButtonStyle(kind: .primary, fullWidth: true))
            Text("会打开「快捷指令」，点「添加快捷指令」就完成了").font(.oink(12.5)).foregroundStyle(oink.muted)
                .multilineTextAlignment(.center)
                .frame(maxWidth: .infinity)
        }
        .padding(.top, -4)
        VStack(alignment: .leading, spacing: 12) {
            guideTitle("放到顺手的地方")
            VStack(spacing: 0) {
                bind(icon: "sliders-horizontal", "控制中心", "从右上角下拉打开控制中心，长按空白处，点「添加控制」，搜索「快捷指令」，选「快捷指令」控制，再选「Oink 复制」。")
                Rectangle().fill(oink.line).frame(height: 0.5)
                bind(icon: "square-dot", "操作按钮", "打开「设置 › 操作按钮」，滑到「快捷指令」，选「Oink 复制」。没有操作按钮的 iPhone，可以在「设置 › 辅助功能 › 触控 › 轻点背面」中绑定。")
            }
            .background(oink.card, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
        }
        VStack(alignment: .leading, spacing: 18) {
            guideTitle("也可以自己创建")
            step(1, "新建快捷指令", "在「快捷指令」App 里新建，添加 Oink 的「获取最新共享内容」操作。")
            step(2, "拷贝到剪贴板", "在后面添加「拷贝到剪贴板」，起名「Oink 复制」。")
        }
        VStack(spacing: 16) {
            Button {
                if let url = URL(string: "shortcuts://create-shortcut") { UIApplication.shared.open(url) }
            } label: {
                HStack(spacing: 6) { Text("打开快捷指令"); Lucide(name: "external-link", size: 16, color: oink.text2) }
            }
            .buttonStyle(CapsuleButtonStyle(kind: .soft, fullWidth: true))
            HStack(spacing: 5) {
                Lucide(name: "info", size: 13, color: oink.muted)
                Text(AppText.pick("支持文字和图片，文件请在传输页下载", "Supports text and images. Download files from the Transfer page")).font(.oink(12.5)).foregroundStyle(oink.muted)
            }
            .frame(maxWidth: .infinity)
        }
    }

    /// Opens the bundled shortcut under its name. Shortcuts shows its 添加快捷指令 sheet for a
    /// .shortcut file it is handed. iOS does not let an app open a file URL in another app
    /// directly (UIApplication.open reports false for it), so a refusal falls back to the
    /// system share sheet, where 「快捷指令」 takes the file; `shortcuts://import-shortcut`
    /// rejects file URLs (verified on an iOS 26 simulator).
    /// The file is the variant tinted for the current accent; after an accent change the
    /// next tap imports the new colour (a shortcut already added keeps its own).
    private func addShortcut() {
        Haptics.selection()
        guard let resource = ShortcutFile.resource(accent: oink.accentName.rawValue),
              let source = Bundle.main.url(forResource: resource, withExtension: ShortcutFile.fileExtension) else {
            toasts.show(AppText.pick("找不到快捷指令文件。", "The shortcut file is missing."))
            return
        }
        let file: URL
        do { file = try ShortcutFile.stage(source, in: FileManager.default.temporaryDirectory) }
        catch { toasts.show(error.oinkMessage); return }
        UIApplication.shared.open(file) { opened in
            guard !opened, let presenter = QuickLookPresenter.topController() else { return }
            let sheet = UIActivityViewController(activityItems: [file], applicationActivities: nil)
            if let popover = sheet.popoverPresentationController {
                popover.sourceView = presenter.view
                popover.sourceRect = CGRect(x: presenter.view.bounds.midX, y: presenter.view.bounds.midY, width: 0, height: 0)
                popover.permittedArrowDirections = []
            }
            presenter.present(sheet, animated: true)
        }
    }

    private func guideTitle(_ text: String) -> some View {
        Text(text).font(.oink(13, .semibold)).tracking(0.3).foregroundStyle(oink.muted)
    }

    /// A 放到顺手的地方 row: icon tile (like the action blocks above), title and how-to.
    private func bind(icon: String, _ title: String, _ text: String) -> some View {
        HStack(alignment: .top, spacing: 14) {
            Lucide(name: icon, size: 19, color: oink.ink)
                .frame(width: 32, height: 32)
                .background(oink.elev, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                .shadow(color: oink.shadow, radius: 1, y: 1)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(.oink(15.5, .semibold)).foregroundStyle(oink.ink).padding(.top, 4)
                Text(text).font(.oink(13.5)).foregroundStyle(oink.text2).lineSpacing(13.5 * 0.6 - 3)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, 16).padding(.vertical, 14)
    }

    private func block(color: Color, icon: String, stroke: String, _ title: String) -> some View {
        HStack(spacing: 12) {
            Lucide(name: icon, size: 18, stroke: stroke, color: oink.onAccent)
                .frame(width: 32, height: 32)
                .background(color, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
            Text(title).font(.oink(14, .medium)).foregroundStyle(oink.ink)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 14).padding(.vertical, 12)
        .frame(maxWidth: 300)
        .background(oink.elev, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .shadow(color: oink.shadow, radius: 1, y: 1)
    }

    private func step(_ n: Int, _ title: String, _ text: String) -> some View {
        HStack(alignment: .top, spacing: 14) {
            Text("\(n)").font(.oink(13, .semibold)).foregroundStyle(oink.text2)
                .frame(width: 28, height: 28).background(oink.chip, in: Circle())
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(.oink(15.5, .semibold)).foregroundStyle(oink.ink).padding(.top, 3)
                Text(text).font(.oink(13.5)).foregroundStyle(oink.text2).lineSpacing(13.5 * 0.6 - 3)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

/// Open-source licenses: everything the app ships (Rust crates, native code, artwork), from the
/// generated licenses/mobile-ios.json and licenses/texts (build-support/generate-licenses.mjs),
/// bundled by project.yml. The footer repeats the Android robot's CC BY 3.0 attribution.
let androidRobotAttribution = "The Android robot is reproduced or modified from work created and shared by Google and used according to terms described in the Creative Commons 3.0 Attribution License."
let androidTrademark = "Android is a trademark of Google LLC."

struct OpenSourceComponent: Decodable, Hashable {
    let id: String
    let name: String
    let version: String
    let license: String
    /// Attribution and modification notices that are not part of the license texts.
    let notices: [String]
    /// SHA-256 names of the full license/notice texts.
    let texts: [String]
}

enum OpenSourceLicenses {
    private struct Index: Decodable { let components: [OpenSourceComponent] }

    /// The bundled index is part of the build; a missing or malformed one is a packaging bug.
    static let all: [OpenSourceComponent] = {
        guard let url = Bundle.main.url(forResource: "mobile-ios", withExtension: "json") else { fatalError("mobile-ios.json is not bundled") }
        do { return try JSONDecoder().decode(Index.self, from: Data(contentsOf: url)).components }
        catch { fatalError("mobile-ios.json: \(error)") }
    }()

    static func find(_ id: String) -> OpenSourceComponent? { all.first { $0.id == id } }

    static func text(_ sha256: String) -> String {
        guard let url = Bundle.main.url(forResource: sha256, withExtension: "txt", subdirectory: "texts"),
              let text = try? String(contentsOf: url, encoding: .utf8) else { fatalError("license text \(sha256) is not bundled") }
        return text
    }
}

private struct LicensesPage: View {
    let push: (SettingsView.Route) -> Void
    var body: some View {
        SettingsSection(footer: "Oink 使用了以上开源项目，感谢它们的作者。\n\(androidRobotAttribution) \(androidTrademark)") {
            ForEach(OpenSourceLicenses.all, id: \.id) { item in
                ValueRow(title: item.name, sub: "\(item.version) · \(item.license)") { push(.license(item.id)) }
            }
        }
    }
}

private struct LicenseText: View {
    @Environment(\.oink) private var oink
    let id: String
    var body: some View {
        let item = OpenSourceLicenses.find(id)
        VStack(alignment: .leading, spacing: 12) {
            Text(item.map { "\($0.name) \($0.version) · \($0.license)" } ?? id).font(.oink(13.5).monospacedDigit())
            Text(item.map { ($0.notices + $0.texts.map { OpenSourceLicenses.text($0).trimmingCharacters(in: .whitespacesAndNewlines) }).joined(separator: "\n\n") } ?? "")
                .font(.oink(13.5))
                .textSelection(.enabled)
        }
        .foregroundStyle(oink.text2)
        .lineSpacing(13.5 * 0.7 - 3)
        .padding(.horizontal, 4).padding(.vertical, 8)
    }
}
