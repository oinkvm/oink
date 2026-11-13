import SwiftUI

/// 设备 tab: device list, with the add-device page pushed on top.
struct DevicesTab: View {
    @EnvironmentObject private var core: CoreSession
    @EnvironmentObject private var pairing: PairingFlow
    @Environment(\.oink) private var oink
    @Binding var addPresented: Bool
    let onSettings: () -> Void
    @State private var selected: SelectedDevice?
    @State private var error = ""

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                TopBar {
                    SettingsGearButton(action: onSettings)
                } trailing: {
                    AudioToolbarButtons(onError: { error = $0 })
                    ToolbarIconButton(label: requestCount > 0 ? "添加设备，\(requestCount) 个配对请求" : "添加设备",
                                      action: { addPresented = true }) {
                        Lucide(name: "plus", size: 25, color: oink.ink)
                            .frame(width: 48, height: 48)
                            .overlay(alignment: .topTrailing) {
                                if requestCount > 0 { RequestBadge(count: requestCount).offset(x: -3, y: 5) }
                            }
                    }
                }
                // Local Network access denied: no computer can connect (L17).
                SessionBanner()
                if let cards = remoteCards {
                    // Without a session the list is unknown, not empty: the
                    // banner above carries the reason (review M27).
                    if cards.isEmpty && core.sessionProblem != nil {
                        Spacer()
                    } else if cards.isEmpty {
                        DevicesEmptyState { addPresented = true }
                    } else {
                        DeviceList(cards: cards) { selected = SelectedDevice(id: $0.id) }
                    }
                } else {
                    // The session has not produced its first device snapshot yet.
                    Spacer()
                }
            }
            .background(oink.bg)
            .toolbar(.hidden, for: .navigationBar)
            .navigationDestination(isPresented: $addPresented) {
                AddDevicePage(onBack: { addPresented = false })
                    .toolbar(.hidden, for: .navigationBar)
            }
        }
        .sheet(item: $selected) { selection in
            DeviceSheet(deviceID: selection.id)
                .environmentObject(core)
                .oinkThemed()
        }
        .alert("Oink", isPresented: Binding(get: { !error.isEmpty }, set: { if !$0 { error = "" } })) {
            Button("好") { error = "" }
        } message: { Text(error) }
        .task {
            #if DEBUG
            if DesignScene.current != nil {
                core.installUIFixtureIfRequested()
                if let id = DeviceSheetScene.deviceID { selected = SelectedDevice(id: id) }
                return
            }
            #endif
            try? await core.refreshDevices()
        }
    }

    private var requestCount: Int { pairing.requests.count }

    /// Other devices of the group from the core's card snapshot; nil until the
    /// first snapshot arrives.
    private var remoteCards: [IOSDeviceCard]? {
        core.cards.map { $0.cards.filter { !$0.isSelf && $0.device != "this-device" } }
    }
}

struct SelectedDevice: Identifiable { let id: String }

/// No other device in the group yet: centred empty state with an add button
/// (mockup `devices-empty`, styled like the add page's searching state).
private struct DevicesEmptyState: View {
    @Environment(\.oink) private var oink
    let onAdd: () -> Void
    var body: some View {
        VStack(spacing: 0) {
            Lucide(name: "monitor-smartphone", size: 30, color: oink.accentInk)
                .frame(width: 72, height: 72)
                .background(oink.accentSoft, in: Circle())
            Text("还没有其它设备").font(.oink(17, .semibold)).foregroundStyle(oink.ink).padding(.top, 18)
            Text("添加一台电脑或手机，就能共享键鼠、剪贴板、文件和音频。")
                .font(.oink(13.5)).foregroundStyle(oink.muted).multilineTextAlignment(.center)
                .lineSpacing(13.5 * 0.6 - 4).frame(maxWidth: 280).padding(.top, 8)
            Button(action: onAdd) {
                HStack(spacing: 6) { Lucide(name: "plus", size: 18, stroke: "2_2", color: oink.accentInk); Text("添加设备") }
                    .padding(.horizontal, 2)
            }
            .buttonStyle(CapsuleButtonStyle(kind: .primary))
            .padding(.top, 22)
        }
        .padding(.horizontal, 32).padding(.top, 24).padding(.bottom, 40)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// Red request count on the add button (`.ad-plus-badge`).
private struct RequestBadge: View {
    @Environment(\.oink) private var oink
    let count: Int
    var body: some View {
        Text("\(count)")
            .font(.oink(11, .bold).monospacedDigit())
            .foregroundStyle(oink.onAccent)
            .padding(.horizontal, 5)
            .frame(minWidth: 19, minHeight: 19)
            .background(oink.bad, in: Capsule())
            .overlay(Capsule().strokeBorder(oink.bg, lineWidth: 2))
            .accessibilityHidden(true)
    }
}

/// Speaker and microphone of this phone (`.m-audio-indicators`). While on (or failed)
/// each draws the PC icon of that state with its badge, the same as a device card:
/// speaker accent + receive (作为出口), microphone accent + send (发送), warning when
/// it could not start. Off: the plain glyph in the ink colour.
/// The switches drive this phone's audio owner; a failure shows its reason.
private struct AudioToolbarButtons: View {
    @ObservedObject private var speakerActions = SpeakerActions.shared
    @EnvironmentObject private var core: CoreSession
    @Environment(\.oink) private var oink
    let onError: (String) -> Void
    var body: some View {
        let audio = core.localAudio
        let speaker: DeviceControls.SpeakerMode? = speakerActions.forwarding ? .forward : DeviceControls.toolbarSpeaker(on: audio.speakerOn, failed: audio.speakerOn && speakerFailed)
        let microphone = DeviceControls.toolbarMicrophone(on: audio.microphoneOn, failed: audio.microphoneOn && microphoneFailed)
        HStack(spacing: 8) {
            icon(speaker.map { DeviceIndicators.speaker($0) }, glyph: "speaker3")
                .frame(width: 48, height: 48).contentShape(Rectangle())
                .speakerGestures { longPress in
                    if !longPress { Haptics.selection() }
                    speakerActions.press(exitOn: audio.speakerOn, longPress: longPress,
                        preferForward: UserDefaults.standard.string(forKey: SpeakerActions.preferenceKey) == "forward")
                }
                .accessibilityLabel(speakerActions.forwarding ? "扬声器：转发中" : audio.speakerOn ? "扬声器：开" : "扬声器：关")
                .accessibilityIdentifier("audio-toolbar-speaker")
            ToolbarIconButton(label: audio.microphoneOn ? "麦克风：开" : "麦克风：关", action: { toggle(microphone: true) }) {
                icon(microphone.map { DeviceIndicators.microphone($0) }, glyph: "microphone")
            }
        }
        // Permission denied / interrupted: the native audio state carries the reason.
        .onChange(of: audio.issue) { _, issue in if let issue { onError(issue) } }
    }
    /// 24 glyph in the PC icon box, so it stays put when the badge comes and goes.
    private func icon(_ item: CapabilityIndicator?, glyph: String) -> some View {
        PCIcon(glyph: item?.glyph ?? glyph, badge: item?.badge, size: 24,
               color: item.map { oink.tone($0.tone) } ?? oink.ink, toneIsGood: item?.tone == .good)
    }
    #if OINK_MOBILE_CORE_FFI
    private var speakerFailed: Bool { core.localAudio.speaker == .failed }
    private var microphoneFailed: Bool { core.localAudio.microphone == .failed }
    #else
    private var speakerFailed: Bool { false }
    private var microphoneFailed: Bool { false }
    #endif
    private func toggle(microphone: Bool) {
        Haptics.selection()
        let audio = core.localAudio
        Task { @MainActor in
            do {
                if microphone { try await core.setLocalMicrophone(!audio.microphoneOn) }
                else { try await core.setLocalSpeaker(!audio.speakerOn) }
            } catch { onError(error.oinkMessage) }
        }
    }
}

/// Online / offline groups of device rows (`.m-device-list`).
private struct DeviceList: View {
    @Environment(\.oink) private var oink
    let cards: [IOSDeviceCard]
    let onOpen: (IOSDeviceCard) -> Void
    @State private var position = ScrollPosition(edge: .top)

    var body: some View {
        let online = cards.filter { $0.connection != .offline }
        let offline = cards.filter { $0.connection == .offline }
        ScrollView {
            // A group without devices hides its title and count entirely.
            VStack(alignment: .leading, spacing: 28) {
                if !online.isEmpty { group("在线", online) }
                if !offline.isEmpty { group("离线", offline) }
            }
            .frame(maxWidth: 620)
            .frame(maxWidth: .infinity)
            .padding(.horizontal, 20)
            .padding(.top, 12)
            .padding(.bottom, 28)
        }
        .scrollIndicators(.hidden)
        .scrollPosition($position)
        .task {
            #if DEBUG
            if DesignScene.is("devices-end") {
                try? await Task.sleep(for: .milliseconds(300))
                position.scrollTo(edge: .bottom)
            }
            #endif
        }
    }

    private func group(_ title: String, _ items: [IOSDeviceCard]) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            GroupHeading(title: title, count: items.isEmpty ? nil : items.count)
            VStack(spacing: 0) {
                ForEach(Array(items.enumerated()), id: \.element.id) { index, card in
                    Button { Haptics.selection(); onOpen(card) } label: { DeviceRow(card: card) }
                        .buttonStyle(.plain)
                        .overlay(alignment: .top) {
                            if index > 0 { Hairline().padding(.leading, 62).padding(.trailing, 16) }
                        }
                }
            }
        }
    }
}

/// One device row (`.m-device`): logo, name, capability icons or last seen, latency.
struct DeviceRow: View {
    @Environment(\.oink) private var oink
    let card: IOSDeviceCard
    private var offline: Bool { card.connection == .offline }
    private var platform: DevicePlatform { DevicePlatform(os: card.os, name: card.name) }

    var body: some View {
        HStack(spacing: 14) {
            PlatformLogo(platform: platform, color: offline ? oink.faint : nil, privileged: card.androidPrivileged)
                .overlay(alignment: .bottomTrailing) {
                    // Decision 3: an incompatible (older) peer carries the PC's upgrade mark.
                    if card.isProtocolIncompatible {
                        AssetIcon("PCMark-upgrade", size: 15, color: oink.warning)
                            .background(oink.bg, in: Circle())
                            .offset(x: 6, y: 5)
                            .accessibilityHidden(true)
                    }
                }
                .offset(x: -8)
            VStack(alignment: .leading, spacing: 0) {
                Text(card.name)
                    .font(.oink(18, .semibold)).kerning(-0.2)
                    .foregroundStyle(offline ? oink.text2 : oink.ink)
                    .multilineTextAlignment(.leading)
                    .frame(minHeight: 25.2)
                if card.isProtocolIncompatible {
                    Text("版本过低，请升级")
                        .font(.oink(13)).foregroundStyle(oink.muted)
                        .padding(.top, 8)
                } else if offline {
                    Text("上次在线 · \(lastSeenText)")
                        .font(.oink(13)).foregroundStyle(oink.muted)
                        .padding(.top, 8)
                } else {
                    CapabilityIcons(indicators: DeviceIndicators.list(card)).padding(.top, 10)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            if !offline { LiveLatencyView(device: card.device) }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 18)
        .frame(minHeight: 100)
        .contentShape(Rectangle())
        .accessibilityElement(children: .combine)
        .accessibilityLabel(card.isProtocolIncompatible ? "\(card.name)，版本过低，请升级" : "打开 \(card.name) 设备控制")
    }

    private var lastSeenText: String {
        guard let ms = card.lastSeenMs else { return "很久以前" }
        let minutes = max(1, Int(Date().timeIntervalSince1970 - Double(ms) / 1000) / 60)
        if minutes < 60 { return "\(minutes) 分钟前" }
        if minutes < 24 * 60 { return "\(minutes / 60) 小时前" }
        return "\(minutes / (24 * 60)) 天前"
    }
}

/// Status icons under the device name (`.m-capabilities`): 18 glyphs, 13 apart.
struct CapabilityIcons: View {
    @Environment(\.oink) private var oink
    let indicators: [CapabilityIndicator]
    var body: some View {
        HStack(spacing: 13) {
            ForEach(indicators, id: \.self) { item in
                PCIcon(glyph: item.glyph, badge: item.badge, size: 18, color: oink.tone(item.tone), toneIsGood: item.tone == .good)
                    .accessibilityElement()
                    .accessibilityLabel(item.label)
            }
        }
    }
}

/// The device's live latency (CoreSession.latency), when measured.
private struct LiveLatencyView: View {
    @ObservedObject private var latency = CoreSession.shared.latency
    let device: String
    var body: some View {
        if let ms = latency.ms[device] { LatencyView(latency: ms) }
    }
}

/// Latency bars + number + ms (`.m-latency`).
struct LatencyView: View {
    @Environment(\.oink) private var oink
    let latency: Int
    var body: some View {
        HStack(spacing: 0) {
            SignalBars(latency: latency)
            Text("\(latency)").font(.oink(12).monospacedDigit()).foregroundStyle(oink.text2)
                .frame(width: 22.7, alignment: .trailing).padding(.leading, 2)
            Text("ms").font(.oink(10)).foregroundStyle(oink.muted).padding(.leading, 10)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("延迟 \(latency) 毫秒")
    }
}

/// PC indicator rules (mockup `indicator()` / PC `indicators()`).
enum DeviceIndicators {
    static func list(_ card: IOSDeviceCard) -> [CapabilityIndicator] {
        let c = card.controls
        let mobile = DevicePlatform(os: card.os, name: card.name).isMobile
        var items: [CapabilityIndicator] = []
        if !mobile {
            items.append(input("mouse", "鼠标", c.mouse))
            items.append(input("keyboard", "键盘", c.keyboard))
        }
        items.append(speaker(c))
        items.append(microphone(c))
        return items
    }
    /// Badge rules of the PC (`indicators()`): muted forwarding / exit shows mute,
    /// forwarding without an exit shows the warning badge.
    static func speaker(_ c: DeviceControls) -> CapabilityIndicator {
        var item = speaker(c.speaker)
        switch c.speaker {
        case .forward where c.speakerWaiting:
            item = CapabilityIndicator(glyph: "speaker3", tone: .good, badge: .warning, label: "音频：等待音频出口")
        case .forward where c.speakerMuted:
            item = CapabilityIndicator(glyph: "speaker3", tone: .good, badge: .mute, label: "音频：转发静音")
        case .outlet where c.speakerMuted:
            item = CapabilityIndicator(glyph: "speaker3", tone: .accent, badge: .mute, label: "音频：出口静音")
        default: break
        }
        return item
    }
    static func microphone(_ c: DeviceControls) -> CapabilityIndicator {
        if c.mic == .send && c.micMuted {
            return CapabilityIndicator(glyph: "microphone", tone: .accent, badge: .mute, label: "麦克风：发送，已静音")
        }
        return microphone(c.mic)
    }
    static func input(_ glyph: String, _ name: String, _ state: DeviceControls.InputShare) -> CapabilityIndicator {
        switch state {
        case .shared: CapabilityIndicator(glyph: glyph, tone: .good, badge: .wifi, label: "\(name)：共享")
        case .local: CapabilityIndicator(glyph: glyph, tone: .muted, badge: nil, label: "\(name)：本地")
        case .blocked: CapabilityIndicator(glyph: glyph, tone: .bad, badge: .ban, label: "\(name)：禁用")
        case .unavailable: CapabilityIndicator(glyph: glyph, tone: .warn, badge: .warning, label: "\(name)：不可用")
        }
    }
    static func speaker(_ mode: DeviceControls.SpeakerMode) -> CapabilityIndicator {
        switch mode {
        case .local: CapabilityIndicator(glyph: "speaker3", tone: .muted, badge: nil, label: "音频：本地")
        case .forward: CapabilityIndicator(glyph: "speaker3", tone: .good, badge: .send, label: "音频：转发")
        case .outlet: CapabilityIndicator(glyph: "speaker3", tone: .accent, badge: .receive, label: "音频：作为出口")
        case .unavailable: CapabilityIndicator(glyph: "speaker3", tone: .warn, badge: .warning, label: "音频：不可用")
        }
    }
    static func microphone(_ mode: DeviceControls.MicMode) -> CapabilityIndicator {
        switch mode {
        case .local: CapabilityIndicator(glyph: "microphone", tone: .muted, badge: .mute, label: "麦克风：本地")
        case .send: CapabilityIndicator(glyph: "microphone", tone: .accent, badge: .send, label: "麦克风：发送")
        case .receive: CapabilityIndicator(glyph: "microphone", tone: .good, badge: .receive, label: "麦克风：接收")
        case .unavailable: CapabilityIndicator(glyph: "microphone", tone: .warn, badge: .warning, label: "麦克风：不可用")
        }
    }
}
