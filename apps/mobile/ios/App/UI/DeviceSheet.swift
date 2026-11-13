import SwiftUI

/// Design states of the control sheet (DEBUG screenshot scenes).
enum DeviceSheetScene {
    static var deviceID: String? {
        switch DesignScene.current {
        case "sheet", "sheet-focus-mouse", "sheet-focus-gaming", "sheet-focus-speaker", "sheet-kick", "sheet-speaker-exit": "fixture-windowsPc"
        case "sheet-macMini": "fixture-macMini"
        case "sheet-mobile": "fixture-iphone"
        case "sheet-android-privileged": "fixture-android"
        case "sheet-offline", "sheet-edit-ip": "fixture-desktop"
        default: nil
        }
    }
    static var focus: DeviceSheet.Control? {
        switch DesignScene.current {
        case "sheet-focus-mouse": .mouse
        case "sheet-focus-gaming": .gaming
        case "sheet-focus-speaker": .speaker
        default: nil
        }
    }
}

/// Device control sheet (mockup `DeviceSheet.jsx`), drawn in the iOS Control Center
/// style on the native sheet glass: round glass toggles that fill with their state colour,
/// and tall capsule volume sliders. Long press opens the mode choices of one control.
struct DeviceSheet: View {
    enum Control: String, CaseIterable, Identifiable {
        case mouse, keyboard, locked, gaming, speaker, mic
        var id: String { rawValue }
        var label: String {
            switch self { case .mouse: "鼠标"; case .keyboard: "键盘"; case .locked: "指针锁定"; case .gaming: "游戏模式"; case .speaker: "扬声器"; case .mic: "麦克风" }
        }
        var isAudio: Bool { self == .speaker || self == .mic }
    }

    // Vertical geometry of the Android sheet (`DeviceSheet.kt`, user 2026-11-11: same on both).
    /// Sheet top to heading: Android's 28 grabber row plus the heading's 14 top padding.
    static let topMargin: CGFloat = 28 + 14
    /// Content to sheet bottom: 16, the 18 status line, 8 (Android adds the navigation-bar
    /// inset below; the floating iOS sheet already sits clear of the home indicator).
    static let bottomMargin: CGFloat = 16 + 18 + 8
    /// The status line's centre below the content.
    static let noticeCenter: CGFloat = 16 + 18 / 2

    @EnvironmentObject private var core: CoreSession
    @Environment(\.oink) private var oink
    @Environment(\.dismiss) private var dismiss
    let deviceID: String
    @State private var loaded = false
    @State private var focus: Control?
    @State private var notice = ""
    @State private var noticeTask: Task<Void, Never>?
    /// Volume while the finger is on a capsule; committed to the core on release.
    @State private var dragLevel: [Control: Int] = [:]
    @State private var confirmKick = false
    @State private var editIP = false
    @State private var ipText = ""
    @State private var portText = ""
    @State private var ipError = ""
    /// The control flying between its grid cell and the mode page (see `FlyIn`).
    @State private var flight: Flight?
    /// Height of the grid page; the mode page keeps it so the sheet never resizes.
    @State private var pageHeight: CGFloat = 0
    @State private var gridWidth: CGFloat = 0
    @State private var frames = ControlFrames()
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var card: IOSDeviceCard? { core.cards?.cards.first { $0.device == deviceID } }
    private var name: String { card?.name ?? "设备" }
    private var platform: DevicePlatform { DevicePlatform(os: card?.os ?? "", name: name) }
    private var offline: Bool { card?.connection == .offline }
    /// The core's state, with the capsule a finger is dragging shown at its live value.
    private var controls: DeviceControls {
        var value = card?.controls ?? DeviceControls()
        if let level = dragLevel[.speaker] { value.speakerLevel = level }
        if let level = dragLevel[.mic] { value.micLevel = level }
        return value
    }

    var body: some View {
        VStack(spacing: 0) {
            // The mode page replaces the whole page at once (mockup `DeviceSheet.jsx`); only
            // the held control moves, as a transform (`FlyIn`), so nothing is laid out again
            // per frame and the sheet keeps its height.
            if let focus {
                // A design scene opening straight into the mode page has no grid height yet.
                FocusView(height: pageHeight > 0 ? pageHeight : 285, control: focus, controls: controls,
                          flight: flight?.control == focus ? flight : nil, frames: frames, onLanded: landed,
                          onChoose: choose, onLevel: dragged, onCommit: commitLevel, onClose: closeFocus)
            } else {
                VStack(spacing: 0) {
                    heading
                    content
                }
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { pageHeight = $0 }
                // The status line lives in the bottom margin, so it never resizes the sheet.
                .overlay(alignment: .bottom) {
                    Text(notice)
                        .font(.oink(11)).foregroundStyle(oink.muted)
                        .lineLimit(1)
                        .alignmentGuide(.bottom) { $0[VerticalAlignment.center] - Self.noticeCenter }
                }
            }
        }
        .coordinateSpace(.named(FlyIn.space))
        .fittedSheet(top: Self.topMargin, bottom: Self.bottomMargin)
        .onAppear(perform: load)
        .alert("将 \(name) 踢出设备组", isPresented: $confirmKick) {
            Button("取消", role: .cancel) {}
            Button("踢出", role: .destructive) { kick() }
        } message: { Text("踢出后，需要重新配对才能再次和它共享") }
        .alert("编辑 IP", isPresented: $editIP) {
            TextField("IP 地址", text: $ipText).keyboardType(.numbersAndPunctuation).autocorrectionDisabled()
            TextField("47470", text: $portText).keyboardType(.numberPad)
            Button("取消", role: .cancel) { ipError = "" }
            Button("确认") { saveIP() }
        } message: { if !ipError.isEmpty { Text(ipError) } }
    }

    // MARK: Heading

    private var heading: some View {
        HStack(spacing: 12) {
            PlatformLogo(platform: platform, color: offline ? oink.faint : nil, privileged: card?.androidPrivileged == true)
            Text(name).font(.oink(20, .semibold)).foregroundStyle(oink.ink)
                .lineLimit(2).frame(maxWidth: .infinity, alignment: .leading)
            HStack(spacing: 4) {
                // A phone has no paused state: only kick (user 2026-11-11).
                if !platform.isMobile {
                    headingButton(controls.paused ? "继续" : "暂停") {
                        if controls.paused { Lucide(name: "play", size: 22, color: oink.bad) }
                        else { AssetIcon("PCAction-pause", size: 22, color: oink.bad) }
                    } action: {
                        Haptics.selection()
                        let paused = !controls.paused
                        run(paused ? "已暂停" : "已继续") { try await core.setDevicePaused(deviceID, paused) }
                    }
                }
                headingButton("踢出设备") {
                    AssetIcon("PCAction-leave", size: 22, color: oink.bad)
                } action: { confirmKick = true }
            }
        }
        .frame(minHeight: 44)
    }

    private func headingButton<Icon: View>(_ label: String, @ViewBuilder icon: () -> Icon, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            // Plain icons, no circle behind them (user 2026-11-08).
            icon().frame(width: 44, height: 44).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(label)
    }

    // MARK: Content

    @ViewBuilder private var content: some View {
        if platform.isMobile {
            Text("无法远程控制此设备")
                .font(.oink(13)).foregroundStyle(oink.faint)
                .frame(maxWidth: .infinity, minHeight: 150)
                .padding(.top, 24)
        } else if offline {
            VStack(spacing: 20) {
                Text(card?.isProtocolIncompatible == true ? "版本过低，请升级" : "上次在线：\(lastSeen)")
                    .font(.oink(14)).foregroundStyle(oink.faint)
                Button(action: openEditIP) {
                    Text("编辑 IP").font(.oink(14)).foregroundStyle(.white)
                        .frame(minWidth: 132 - 48).padding(.horizontal, 24).frame(height: 44)
                        .sheetGlass(in: Capsule())
                }
                .buttonStyle(.plain)
            }
            .frame(maxWidth: .infinity, minHeight: 176)
            .padding(.top, 24)
        } else {
            grid.padding(.top, 36)
        }
    }

    private var grid: some View {
        GeometryReader { geo in
            let column = (geo.size.width - 36) / 4
            HStack(alignment: .top, spacing: 12) {
                VStack(spacing: 20) {
                    round(.mouse, column); round(.locked, column)
                }
                .zIndex(returning([.mouse, .locked]))
                VStack(spacing: 20) {
                    round(.keyboard, column); round(.gaming, column)
                }
                .zIndex(returning([.keyboard, .gaming]))
                audio(.speaker, column, column * 2 + 20).zIndex(returning([.speaker]))
                audio(.mic, column, column * 2 + 20).zIndex(returning([.mic]))
            }
        }
        // Exactly the two rows (2 × column + 20), so no empty band is left under the grid
        // on narrower phones.
        .frame(height: gridWidth > 0 ? (gridWidth - 36) / 2 + 20 : 188)
        .frame(maxWidth: 420)
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { gridWidth = $0 }
    }

    /// The column of a control flying back from the mode page is drawn above the others.
    private func returning(_ column: [Control]) -> Double {
        flight.map { column.contains($0.control) } == true ? 1 : 0
    }

    private func round(_ control: Control, _ size: CGFloat) -> some View {
        RoundToggle(control: control, controls: controls, size: size, disabled: offline,
                    onTap: { tap(control) }, onHold: { open(control, pressScale: $0) })
            .modifier(FlyIn(flight: flight?.control == control ? flight : nil, onLanded: landed))
            .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(FlyIn.space)) } action: { frames.grid[control] = $0 }
    }

    private func audio(_ control: Control, _ width: CGFloat, _ height: CGFloat) -> some View {
        VolumeCapsule(control: control, controls: controls, disabled: offline,
                      onLevel: { dragged(control, $0) }, onCommit: { commitLevel(control, $0) },
                      onHold: { open(control, pressScale: $0) })
            .frame(width: width, height: height)
            .modifier(FlyIn(flight: flight?.control == control ? flight : nil, onLanded: landed))
            .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(FlyIn.space)) } action: { frames.grid[control] = $0 }
    }

    // MARK: Behaviour

    private var lastSeen: String {
        guard let ms = card?.lastSeenMs else { return "很久以前" }
        let minutes = max(1, Int(Date().timeIntervalSince1970 - Double(ms) / 1000) / 60)
        return minutes < 60 ? "\(minutes) 分钟前" : minutes < 1440 ? "\(minutes / 60) 小时前" : "\(minutes / 1440) 天前"
    }

    private func load() {
        guard !loaded else { return }
        loaded = true
        #if DEBUG
        if DeviceSheetScene.deviceID == deviceID {
            focus = DeviceSheetScene.focus
            if DesignScene.is("sheet-kick") { confirmKick = true }
            if DesignScene.is("sheet-edit-ip") { editIP = true }
        }
        #endif
    }

    /// Game mode shows as on when it is in effect (an Auto preference included).
    private var gamingOn: Bool { controls.gaming == .on || (controls.gaming == .auto && controls.gameActive) }

    private func tap(_ control: Control) {
        switch control {
        case .mouse: choose(.mouse, controls.mouse == .shared ? "local" : "shared")
        case .keyboard: choose(.keyboard, controls.keyboard == .shared ? "local" : "shared")
        case .locked: choose(.locked, controls.locked ? "off" : "on")
        // A tap flips what is in effect and leaves Auto; only the mode page selects Auto.
        case .gaming: choose(.gaming, gamingOn ? "off" : "on")
        case .speaker, .mic: break
        }
    }

    /// Long press: the control leaves its cell at the size the press grew or shrank it to
    /// (`pressScale`) and settles in the mode page at its normal size.
    private func open(_ control: Control, pressScale: CGFloat) {
        Haptics.long()
        flight = reduceMotion ? nil : frames.grid[control].map { Flight(control: control, from: $0.scaled(by: pressScale)) }
        focus = control
    }

    /// Back to the grid; the control flies from the mode page to its cell.
    private func closeFocus() {
        guard let control = focus else { return }
        flight = reduceMotion ? nil : frames.focus.map { Flight(control: control, from: $0) }
        focus = nil
    }

    /// A landed flight is dropped, so a page inserted later (an offline device coming
    /// back, say) does not replay it.
    private func landed(_ done: Flight) {
        if flight == done { flight = nil }
    }

    private static let words = ["on": "开", "off": "关", "auto": "自动", "local": "本地", "shared": "共享", "blocked": "已禁用",
                                "forward": "转发", "outlet": "作为出口", "receive": "接收", "send": "发送"]

    /// Applies one mode choice through the core. The tiles show the core's
    /// state, so they change only once the device confirmed the change.
    private func choose(_ control: Control, _ value: String) {
        Haptics.selection()
        let id = deviceID
        // In the mode page a choice applies it and returns to the grid, as tapping empty space does.
        run("\(control.label)：\(Self.words[value] ?? value)") {
            switch control {
            case .mouse: try await core.setMouse(id, Self.input(value))
            case .keyboard: try await core.setKeyboard(id, Self.input(value))
            case .locked: try await core.setFocusLock(id, locked: value == "on")
            case .gaming: try await core.setGameMode(id, value == "auto" ? .auto : value == "on" ? .on : .off)
            case .speaker: try await core.setSpeakerMode(id, DeviceControls.SpeakerMode(rawValue: value) ?? .local)
            case .mic: try await core.setMicrophoneMode(id, value == "send" ? .share : value == "receive" ? .receive : .local)
            }
        }
    }

    private static func input(_ value: String) -> IOSInputChoice {
        switch value { case "shared": .shared; case "blocked": .disabled; default: .local }
    }

    /// Live value while dragging a capsule (not sent yet).
    private func dragged(_ control: Control, _ value: Int) {
        dragLevel[control] = value
    }

    /// Finger lifted (or an accessibility step): send the volume once.
    private func commitLevel(_ control: Control, _ value: Int) {
        let mode = control == .speaker ? controls.speaker.rawValue : controls.mic.rawValue
        let words = ["local": "本地", "forward": "转发", "outlet": "作为出口", "receive": "接收", "send": "发送"]
        let id = deviceID
        run("\(control.label)：\(words[mode] ?? mode) · \(value)%", after: { dragLevel[control] = nil }) {
            if control == .speaker { try await core.setSpeakerLevel(id, percent: value) }
            else { try await core.setMicrophoneLevel(id, percent: value) }
        }
    }

    /// Runs one control call: the notice shows `success` once the core
    /// confirmed it, otherwise the reason the core gave.
    private func run(_ success: String, after: @escaping @MainActor () -> Void = {}, _ action: @escaping @MainActor () async throws -> Void) {
        Task { @MainActor in
            do {
                try await action()
                show(success)
            } catch {
                Haptics.error()
                show(error.oinkMessage)
            }
            after()
        }
    }

    private func show(_ text: String) {
        noticeTask?.cancel()
        notice = text
        noticeTask = Task { @MainActor in
            try? await Task.sleep(for: .seconds(2))
            if !Task.isCancelled { notice = "" }
        }
    }

    private func kick() {
        Task { @MainActor in
            do {
                try await core.removeMember(deviceID)
                dismiss()
            } catch {
                Haptics.error()
                show(error.oinkMessage)
            }
        }
    }

    /// Prefills the dialog with the address the phone uses for this member.
    private func openEditIP() {
        ipError = ""
        Task { @MainActor in
            let current = (try? await core.memberAddresses(deviceID))?.first ?? ""
            let (ip, port) = Self.split(current)
            ipText = ip
            portText = port == "47470" ? "" : port
            editIP = true
        }
    }

    private static func split(_ value: String) -> (String, String) {
        if value.hasPrefix("["), let end = value.firstIndex(of: "]") {
            let host = String(value[value.index(after: value.startIndex)..<end])
            return (host, String(value[value.index(after: end)...].drop(while: { $0 == ":" })))
        }
        let parts = value.split(separator: ":", omittingEmptySubsequences: false)
        return parts.count == 2 ? (String(parts[0]), String(parts[1])) : (value, "")
    }

    private func saveIP() {
        let ip = ipText.trimmingCharacters(in: .whitespaces), port = portText.trimmingCharacters(in: .whitespaces)
        let ipv4 = ip.split(separator: ".", omittingEmptySubsequences: false).count == 4
            && ip.split(separator: ".", omittingEmptySubsequences: false).allSatisfy { Int($0).map { $0 <= 255 } == true && $0.count <= 3 }
        let ipv6 = ip.contains(":") && ip.allSatisfy { $0.isHexDigit || $0 == ":" || $0 == "." }
        if ip.isEmpty { ipError = "请输入 IP 地址"; editIP = true; return }
        if !ipv4 && !ipv6 { ipError = "请输入有效的 IP 地址"; editIP = true; return }
        if !port.isEmpty, Int(port).map({ (1...65535).contains($0) }) != true { ipError = "端口需为 1–65535，或留空"; editIP = true; return }
        ipError = ""
        let address = port.isEmpty ? ip : ipv6 ? "[\(ip)]:\(port)" : "\(ip):\(port)"
        // Only a return from the core means the address was stored.
        run("IP 已更新") { try await core.setMemberAddress(deviceID, address: address) }
    }
}

// MARK: - Round toggle

/// Control Center style round toggle: glass when off / local, filled with the state colour
/// (shared green, disabled red, on = theme accent) with a white glyph when active.
private struct RoundToggle: View {
    @Environment(\.oink) private var oink
    let control: DeviceSheet.Control
    let controls: DeviceControls
    let size: CGFloat
    var disabled = false
    var onTap: () -> Void = {}
    /// Called after the 450 ms hold with the scale the press grew the toggle to.
    var onHold: (CGFloat) -> Void = { _ in }
    @State private var pressScale: CGFloat = 1
    /// Identifies the current press, so a late second grow step of an ended press is dropped.
    @State private var press = 0
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// Hold feedback of the mockup (`usePress`): 1 → 1.08 in the first 16 % of the hold,
    /// then on to 1.18 when the hold fires, both linear; released early it eases back in 140 ms.
    static let holdScale: CGFloat = 1.18

    var body: some View {
        let state = Self.state(control, controls, oink: oink)
        ZStack {
            if state.active {
                Circle().fill(.white)
            } else {
                Circle().fill(.clear).sheetGlass(in: Circle(), interactive: false)
            }
            glyph(state)
        }
        .frame(width: size, height: size)
        .scaleEffect(pressScale)
        .animation(.easeOut(duration: 0.22), value: state.active)
        .opacity(disabled ? 0.45 : 1)
        .contentShape(Circle())
        .gesture(LongPressGesture(minimumDuration: 0.45)
            .onEnded { _ in onHold(reduceMotion ? 1 : Self.holdScale) }
            .exclusively(before: TapGesture().onEnded { onTap() }))
        .onLongPressGesture(minimumDuration: 0.45, perform: {}, onPressingChanged: pressing)
        .allowsHitTesting(!disabled)
        .accessibilityElement()
        .accessibilityLabel("\(control.label)：\(state.label)")
        .accessibilityAddTraits(.isButton)
        .accessibilityAction(named: "选择模式") { onHold(1) }
    }

    private func pressing(_ down: Bool) {
        press += 1
        guard !reduceMotion else { return }
        guard down else {
            // A completed hold also ends the press here, but the sheet has already swapped
            // this toggle for the mode page, whose copy takes over at `holdScale`.
            withAnimation(.easeOut(duration: 0.14)) { pressScale = 1 }
            return
        }
        let current = press
        withAnimation(.linear(duration: 0.072)) { pressScale = 1.08 } completion: {
            guard press == current else { return }
            withAnimation(.linear(duration: 0.378)) { pressScale = Self.holdScale }
        }
    }

    @ViewBuilder private func glyph(_ state: ToggleState) -> some View {
        // Active: the state colour on white; inactive: white on glass.
        let color = state.active ? state.tint : Color.white
        switch control {
        case .locked:
            Lucide(name: "lock-keyhole", size: 28, color: color)
        case .gaming:
            ZStack(alignment: .topLeading) {
                PCIcon(glyph: "game", size: 28, color: color)
                    .frame(width: 42, height: 32, alignment: .topLeading)
                    .mask {
                        KnockOut(hole: controls.gaming == .auto ? CGRect(x: 14.5, y: 19.5, width: 27, height: 12) : nil, radius: 3)
                            .fill(style: FillStyle(eoFill: true))
                    }
                if controls.gaming == .auto {
                    Text("AUTO").font(.system(size: 8, weight: .bold)).kerning(0.2).foregroundStyle(color)
                        .frame(width: 27, height: 12)
                        .offset(x: 31.5 - 27 + 10, y: 31.5 - 12)
                }
            }
            .frame(width: 31.5, height: 31.5, alignment: .topLeading)
        default:
            PCIcon(glyph: control == .mouse ? "mouse" : "keyboard", badge: state.badge, size: 28, color: color, toneIsGood: true)
        }
    }

    struct ToggleState {
        var fill: Color?; var tint: Color; var badge: PCBadgeName?; var label: String
        /// Grey / local / off states are inactive; the warning state counts as active.
        var active: Bool { fill != nil || badge == .warning }
    }

    static func state(_ control: DeviceSheet.Control, _ c: DeviceControls, oink: OinkPalette) -> ToggleState {
        switch control {
        case .mouse, .keyboard:
            switch control == .mouse ? c.mouse : c.keyboard {
            case .shared: return ToggleState(fill: oink.good, tint: oink.good, badge: .wifi, label: "共享")
            case .blocked: return ToggleState(fill: oink.bad, tint: oink.bad, badge: .ban, label: "禁用")
            case .unavailable: return ToggleState(fill: nil, tint: oink.warning, badge: .warning, label: "不可用")
            case .local: return ToggleState(fill: nil, tint: oink.faint, badge: nil, label: "本地")
            }
        case .locked:
            return c.locked ? ToggleState(fill: oink.accent, tint: oink.accent, label: "开启") : ToggleState(fill: nil, tint: oink.faint, label: "关闭")
        case .gaming:
            let on = c.gaming == .on || (c.gaming == .auto && c.gameActive)
            let label = c.gaming == .auto ? "自动" : on ? "开启" : "关闭"
            return on ? ToggleState(fill: oink.accent, tint: oink.accent, label: label) : ToggleState(fill: nil, tint: oink.faint, label: label)
        case .speaker, .mic:
            return ToggleState(fill: nil, tint: oink.faint, label: "")
        }
    }
}

// MARK: - Volume capsule

/// Control Center style volume slider: a glass capsule that fills from the bottom in the
/// mode colour (forward / receive green, exit / send accent). Local mode is read-only at 0.
/// Maximum as on the PC: speaker 100 while this device is the exit, otherwise 200, with the
/// 97–103 → 100 detent (haptic only) on a 200 bar.
private struct VolumeCapsule: View {
    @Environment(\.oink) private var oink
    let control: DeviceSheet.Control
    let controls: DeviceControls
    var disabled = false
    /// Live value while the finger moves (shown only).
    var onLevel: (Int) -> Void = { _ in }
    /// Final value when the finger lifts; the one value sent to the core.
    var onCommit: (Int) -> Void = { _ in }
    /// Called after the 450 ms hold with the scale the press shrank the capsule to.
    var onHold: (CGFloat) -> Void = { _ in }
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// Press feedback of a draggable control (mockup `usePress` with onDrag): after 180 ms
    /// still, it shrinks to 0.92 over 270 ms, linear, and the hold fires when it gets there.
    static let holdScale: CGFloat = 0.92
    @State private var start: Date?
    @State private var moved = false
    @State private var held = false
    @State private var inDetent = false
    @State private var holdTask: Task<Void, Never>?
    @State private var pressed = false
    @State private var lastLevel: Int?

    private var isSpeaker: Bool { control == .speaker }
    private var local: Bool { isSpeaker ? controls.speaker == .local : controls.mic == .local }
    private var unavailable: Bool { isSpeaker ? controls.speaker == .unavailable : controls.mic == .unavailable }
    private var max: Int { !isSpeaker ? 200 : controls.speaker == .outlet ? 100 : 200 }
    private var value: Int { local ? 0 : min(isSpeaker ? controls.speakerLevel : controls.micLevel, max) }
    private var tone: Color {
        if unavailable { return oink.warning }
        if local { return oink.faint }
        let green = isSpeaker ? controls.speaker == .forward : controls.mic == .receive
        return green ? oink.good : oink.accent
    }
    private var badge: PCBadgeName? {
        if unavailable { return .warning }
        if local { return isSpeaker ? nil : .mute }
        if isSpeaker {
            if controls.speakerWaiting { return .warning }
            if controls.speakerMuted { return .mute }
            return controls.speaker == .forward ? .send : .receive
        }
        if controls.micMuted { return .mute }
        return controls.mic == .send ? .send : .receive
    }

    var body: some View {
        GeometryReader { geo in
            let fraction = CGFloat(value) / CGFloat(max)
            let covered = fraction > 0.14
            ZStack(alignment: .bottom) {
                Capsule().fill(.clear).sheetGlass(in: Capsule(), interactive: false)
                Rectangle().fill(.white)
                    .frame(height: geo.size.height * fraction)
                PCIcon(glyph: isSpeaker ? "speaker\(volumeWaves(value))" : "microphone", badge: badge, size: 28,
                       color: local ? .white : tone, volume: true, toneIsGood: true)
                    .padding(.bottom, 20)
            }
            .clipShape(Capsule())
            .scaleEffect(pressed ? Self.holdScale : 1)
            .contentShape(Capsule())
            .gesture(DragGesture(minimumDistance: 0).onChanged { g in changed(g, geo.size.height) }
                .onEnded { g in ended(g, geo.size.height) })
        }
        .opacity(disabled ? 0.45 : 1)
        .allowsHitTesting(!disabled)
        .accessibilityElement()
        .accessibilityLabel("\(control.label)音量")
        .accessibilityValue(local ? "本地，音量固定为零" : "\(value)%")
        .accessibilityAdjustableAction { direction in
            guard !local else { return }
            let next = Swift.max(0, Swift.min(max, value + (direction == .increment ? 5 : -5)))
            onLevel(next)
            onCommit(next)
        }
        .accessibilityAction(named: "选择模式") { onHold(1) }
    }

    private func changed(_ g: DragGesture.Value, _ height: CGFloat) {
        if start == nil {
            start = Date(); moved = false; held = false
            holdTask = Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(180))
                guard !Task.isCancelled else { return }
                if !reduceMotion { withAnimation(.linear(duration: 0.27)) { pressed = true } }
                try? await Task.sleep(for: .milliseconds(270))
                guard !Task.isCancelled, !moved else { return }
                held = true
                // The sheet swaps this capsule for the mode page now; its copy starts at the
                // pressed size, so this one is not grown back first.
                onHold(reduceMotion ? 1 : Self.holdScale)
            }
        }
        guard !held else { return }
        if !moved, hypot(g.translation.width, g.translation.height) > 8 {
            moved = true
            holdTask?.cancel()
            withAnimation(.easeOut(duration: 0.14)) { pressed = false }
        }
        if moved { apply(g.location.y, height) }
    }

    private func ended(_ g: DragGesture.Value, _ height: CGFloat) {
        holdTask?.cancel()
        withAnimation(.easeOut(duration: 0.14)) { pressed = false }
        if !held && !moved { apply(g.location.y, height) }
        if !held, let lastLevel { onCommit(lastLevel) }
        start = nil; inDetent = false; lastLevel = nil
    }

    private func apply(_ y: CGFloat, _ height: CGFloat) {
        guard !local else { return }
        let raw = Int((Swift.max(0, Swift.min(1, (height - y) / height)) * CGFloat(max)).rounded())
        let detent = max == 200 && raw >= 97 && raw <= 103
        if detent && !inDetent { Haptics.selection() }
        inDetent = detent
        let mapped: Int
        if max != 200 { mapped = raw }
        else if detent { mapped = 100 }
        else if raw < 97 { mapped = Int((Double(raw) * 99 / 96).rounded()) }
        else { mapped = Int((101 + Double(raw - 104) * 99 / 96).rounded()) }
        let level = Swift.max(0, Swift.min(max, mapped))
        lastLevel = level
        onLevel(level)
    }
}

// MARK: - Focus (mode choice)

/// Long-press detail: the control on the left quarter axis, its label below, and the mode
/// capsules on the right half. Choosing a mode applies it and returns to the grid, and so
/// does tapping empty space.
private struct FocusView: View {
    @Environment(\.oink) private var oink
    /// Room for the label under the control (12 gap + 20 line).
    static let labelSpace: CGFloat = 32
    /// The grid page's height, so the sheet keeps its size when the page swaps.
    let height: CGFloat
    let control: DeviceSheet.Control
    let controls: DeviceControls
    let flight: Flight?
    let frames: ControlFrames
    let onLanded: (Flight) -> Void
    let onChoose: (DeviceSheet.Control, String) -> Void
    let onLevel: (DeviceSheet.Control, Int) -> Void
    let onCommit: (DeviceSheet.Control, Int) -> Void
    let onClose: () -> Void

    private var options: [(String, String, PCBadgeName?)] {
        switch control {
        case .mouse, .keyboard: [("shared", "共享", .wifi), ("local", "本地", nil), ("blocked", "禁用", .ban)]
        case .locked: [("on", "开启", nil), ("off", "关闭", nil)]
        case .gaming: [("on", "开启", nil), ("off", "关闭", nil), ("auto", "自动", nil)]
        case .speaker: [("local", "本地", nil), ("forward", "转发到出口", nil), ("outlet", "作为出口", nil)]
        case .mic: [("local", "本地", nil), ("receive", "接收", nil), ("send", "发送", nil)]
        }
    }
    private var current: String {
        switch control {
        case .mouse: controls.mouse.rawValue
        case .keyboard: controls.keyboard.rawValue
        case .locked: controls.locked ? "on" : "off"
        case .gaming: controls.gaming.rawValue
        case .speaker: controls.speaker.rawValue
        case .mic: controls.mic.rawValue
        }
    }

    /// Selected option colour by meaning (PC state table): shared / forward / receive green,
    /// disabled red, local / off neutral ink, everything else the theme accent.
    private func optionColor(_ value: String) -> Color {
        switch value {
        case "shared", "forward", "receive": oink.good
        case "blocked": oink.bad
        // Neutral options on the white selected capsule are black in both themes.
        case "local", "off": Color(hex: 0x1B1C22)
        default: oink.accent
        }
    }

    var body: some View {
        // Control and options share one horizontal axis through the middle of the page
        // (mockup `.cc-focus`: align-items center, label positioned under the control).
        HStack(spacing: 0) {
            Group {
                if control.isAudio {
                    VolumeCapsule(control: control, controls: controls, onLevel: { onLevel(control, $0) }, onCommit: { onCommit(control, $0) })
                        // 218 tall, shorter when the page is low so the label still fits below.
                        .frame(width: 76, height: min(218, height - 2 * Self.labelSpace))
                } else {
                    RoundToggle(control: control, controls: controls, size: 80)
                        .allowsHitTesting(false)
                }
            }
            .modifier(FlyIn(flight: flight, onLanded: onLanded))
            .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(FlyIn.space)) } action: { frames.focus = $0 }
            .overlay(alignment: .bottom) {
                Text(control.label).font(.oink(13)).foregroundStyle(oink.text2).frame(height: 20)
                    .fixedSize()
                    .alignmentGuide(.bottom) { $0[.top] - 12 }
            }
            .frame(maxWidth: .infinity)
            // The arriving control passes over the option column.
            .zIndex(1)
            VStack(spacing: 14) {
                ForEach(options, id: \.0) { value, label, badge in
                    let selected = current == value
                    Button { onChoose(control, value); onClose() } label: {
                        let ink = selected ? optionColor(value) : Color.white
                        HStack(spacing: 7) {
                            if control == .mouse || control == .keyboard {
                                PCIcon(glyph: control == .mouse ? "mouse" : "keyboard", badge: badge, size: 18,
                                       color: ink, toneIsGood: true)
                            }
                            Text(label).font(.oink(14, selected ? .semibold : .regular))
                        }
                        .foregroundStyle(ink)
                        .frame(maxWidth: .infinity).frame(height: 48)
                        .background { if selected { Capsule().fill(.white) } }
                        .sheetGlass(in: Capsule())
                        .opacity(1)
                    }
                    .buttonStyle(.plain)
                    .accessibilityAddTraits(selected ? [.isSelected] : [])
                }
            }
            .padding(.leading, 4).padding(.trailing, 8)
            .offset(x: -4)
            .frame(maxWidth: .infinity)
        }
        .frame(height: height)
        .contentShape(Rectangle())
        .onTapGesture { onClose() }
    }
}

// MARK: - Flight between the grid and the mode page

/// One move of a control between its grid cell and the mode page; `from` is the rect it
/// is seen at when the move starts, in `FlyIn.space`.
struct Flight: Equatable {
    let control: DeviceSheet.Control
    let from: CGRect
    let id = UUID()
}

/// Layout frames of the controls, kept outside SwiftUI state: they are read only when a
/// flight starts, so recording them must not re-render the sheet.
final class ControlFrames {
    var grid: [DeviceSheet.Control: CGRect] = [:]
    var focus: CGRect?
}

/// FLIP move of the mockup (320 ms, cubic-bezier(.22,1,.36,1)): the view is laid out at its
/// destination and drawn from `flight.from` back to its own frame by an offset and a scale.
/// Both are render transforms, so the move costs no layout per frame and continues the
/// press scale without a jump.
struct FlyIn: ViewModifier {
    static let space = "device-sheet"
    let flight: Flight?
    var onLanded: (Flight) -> Void = { _ in }
    @State private var progress: CGFloat = 1

    func body(content: Content) -> some View {
        content
            .modifier(FlightEffect(progress: progress, from: flight?.from))
            // A flight is only ever handed to a view as it is inserted (the page swap
            // recreates both pages), so it starts here.
            .onAppear {
                guard let flight else { return }
                // The frame that swaps the page (new glass controls and options) takes several
                // display frames to build (measured 40–80 ms on the simulator). Started in that
                // same update, the animation clock runs during the build and the first visible
                // step jumps a fifth of the way; started on the next turn of the main queue, the
                // control holds its pressed place through the build and then moves every frame.
                DispatchQueue.main.async {
                    withAnimation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.32)) { progress = 0 } completion: {
                        onLanded(flight)
                    }
                }
            }
    }
}

/// The transform of one flight frame. Animatable, so SwiftUI hands it each interpolated
/// `progress`; the offset and scale toward the view's own frame are worked out in the render
/// phase (`visualEffect`), which does not lay anything out again.
private struct FlightEffect: ViewModifier, Animatable {
    var progress: CGFloat
    let from: CGRect?
    var animatableData: CGFloat {
        get { progress }
        set { progress = newValue }
    }

    func body(content: Content) -> some View {
        content.visualEffect { [from, progress] effect, proxy in
            let to = proxy.frame(in: .named(FlyIn.space))
            let p = from == nil || to.width <= 0 || to.height <= 0 ? 0 : progress
            let source = from ?? to
            return effect
                .scaleEffect(x: 1 + (source.width / max(to.width, 1) - 1) * p,
                             y: 1 + (source.height / max(to.height, 1) - 1) * p)
                .offset(x: (source.midX - to.midX) * p, y: (source.midY - to.midY) * p)
        }
    }
}

extension CGRect {
    /// This rect scaled by `factor` around its centre.
    func scaled(by factor: CGFloat) -> CGRect {
        insetBy(dx: width * (1 - factor) / 2, dy: height * (1 - factor) / 2)
    }
}
