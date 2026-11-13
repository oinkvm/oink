import SwiftUI
import UIKit

/// 添加设备 page (mockup `AddDevicePage.jsx`): pairing requests, nearby devices and manual add.
/// This phone's own address and ID are in the 本机信息 dialog behind the top-right (i).
struct AddDevicePage: View {
    @EnvironmentObject private var core: CoreSession
    @EnvironmentObject private var pairing: PairingFlow
    @EnvironmentObject private var toasts: ToastCenter
    @Environment(\.oink) private var oink
    let onBack: () -> Void
    @State private var spin = 0.0

    var body: some View {
        VStack(spacing: 0) {
            TopBar(title: "添加设备") {
                // Icon-only back in the gear's slot (same 48 target, 23 icon).
                ToolbarIconButton(label: "返回", action: onBack) { Lucide(name: "chevron-left", size: 23, color: oink.ink) }
            } trailing: {
                ToolbarIconButton(label: "本机信息", action: { Haptics.selection(); pairing.showIPs = true }) {
                    Lucide(name: "info", size: 23, color: oink.ink)
                }
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    if !pairing.requests.isEmpty { requestsSection.padding(.bottom, 28) }
                    nearbySection
                }
                .frame(maxWidth: 620)
                .frame(maxWidth: .infinity)
                .padding(.horizontal, 20)
                .padding(.top, 12)
                .padding(.bottom, 28)
            }
            .scrollIndicators(.hidden)
        }
        .background(oink.bg)
        .fullScreenCover(isPresented: $pairing.showIPs) {
            InfoDialog(showAll: DesignScene.is("add-info-all"))
                .environmentObject(core)
                .oinkThemed()
                .presentationBackground(.clear)
        }
        // The dialog fades in over the page instead of sliding up.
        .transaction { if pairing.showIPs { $0.disablesAnimations = true } }
        .onChange(of: core.foundPeer?.queryId) { _, id in if id != nil { pairing.manualQueryFinished(found: true, problem: nil) } }
        .onChange(of: core.manualAddProblem) { _, problem in if let problem { pairing.manualQueryFinished(found: false, problem: problem.text) } }
    }

    // MARK: Requests

    private var requestsSection: some View {
        VStack(alignment: .leading, spacing: 4) {
            GroupHeading(title: "配对请求", count: pairing.requests.count).frame(minHeight: 36)
            ForEach(Array(pairing.requests.enumerated()), id: \.element.id) { index, request in
                PeerRow(platform: DevicePlatform(os: request.peer.os, name: request.peer.name), name: request.peer.name,
                        sub: requestSubtitle(request)) {
                    HStack(spacing: 10) {
                        roundButton(icon: "x", stroke: "2_2", fill: oink.badSoft, ink: oink.badInk,
                                    label: "拒绝 \(request.peer.name) 的配对请求") { pairing.reject(request) }
                        roundButton(icon: "check", stroke: "2_4", fill: oink.goodSoft, ink: oink.goodInk,
                                    label: "接受 \(request.peer.name) 的配对请求") { pairing.accept(request) }
                    }
                }
                .overlay(alignment: .top) { if index > 0 { Hairline().padding(.leading, 46) } }
            }
        }
    }

    private func requestSubtitle(_ request: PendingRequest) -> String {
        let ip = core.displayAddress(request.peer.device).map { Self.host($0) }
        let group = request.sourceCount > 1 ? "设备组共 \(request.sourceCount) 台" : nil
        return [ip, group].compactMap { $0 }.joined(separator: " · ")
    }

    /// The host part of `ip:port` / `[v6]:port`.
    private static func host(_ address: String) -> String {
        if address.hasPrefix("["), let end = address.firstIndex(of: "]") {
            return String(address[address.index(after: address.startIndex)..<end])
        }
        return address.components(separatedBy: ":").first ?? address
    }

    private func roundButton(icon: String, stroke: String, fill: Color, ink: Color, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Lucide(name: icon, size: 20, stroke: stroke, color: ink)
                .frame(width: 40, height: 40)
                .background(fill, in: Circle())
        }
        .buttonStyle(ShrinkButtonStyle())
        .accessibilityLabel(label)
    }

    // MARK: Nearby

    private var nearbyRows: [NearbyDevice] { core.nearby?.rows ?? [] }

    private var nearbySection: some View {
        VStack(alignment: .leading, spacing: 4) {
            GroupHeading(title: "附近设备", count: nearbyRows.isEmpty ? nil : nearbyRows.count) {
                Button {
                    Haptics.selection()
                    withAnimation(.timingCurve(0.4, 0, 0.2, 1, duration: 0.72)) { spin += 360 }
                    Task { @MainActor in
                        do { try await core.refreshNearby() } catch { toasts.show(error.oinkMessage) }
                    }
                } label: {
                    Lucide(name: "refresh-cw", size: 17, color: oink.muted)
                        .rotationEffect(.degrees(spin))
                        .frame(width: 40, height: 40)
                }
                .buttonStyle(.plain)
                .padding(.trailing, -8)
                .accessibilityLabel("刷新附近设备")
            }
            .frame(minHeight: 36)
            if core.discoveryUnavailable {
                emptyState(unavailable: true)
            } else if nearbyRows.isEmpty {
                emptyState(unavailable: false)
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(nearbyRows.enumerated()), id: \.element.id) { index, row in
                        Button { pairing.connect(row) } label: {
                            PeerRow(platform: DevicePlatform(os: row.os, name: row.name), name: row.name,
                                    sub: row.compatible ? (row.address ?? "") : "协议版本不兼容",
                                    disabled: !row.compatible) {
                                Lucide(name: "plus", size: 20, stroke: "2_2", color: oink.ink)
                                    .frame(width: 40, height: 40)
                                    .background(oink.chip, in: Circle())
                                    .opacity(row.compatible ? 1 : 0.4)
                            }
                        }
                        .buttonStyle(RowButtonStyle())
                        .disabled(!row.compatible)
                        .overlay(alignment: .top) { if index > 0 { Hairline().padding(.leading, 46) } }
                        .accessibilityLabel(row.compatible ? "与 \(row.name) 配对，\(row.address ?? "")" : "\(row.name)，协议版本不兼容")
                    }
                }
                HStack(spacing: 8) {
                    OinkDots()
                    Text("正在搜索附近的设备").font(.oink(12.5))
                }
                .foregroundStyle(oink.faint)
                .frame(maxWidth: .infinity)
                .padding(.top, 14)
            }
            Button(action: pairing.openManual) {
                HStack(spacing: 6) {
                    Lucide(name: "plus", size: 17, stroke: "2_2", color: oink.text2)
                    Text("手动添加")
                }
            }
            .buttonStyle(CapsuleButtonStyle(kind: .soft))
            .frame(maxWidth: .infinity)
            .padding(.top, 16)
        }
    }

    private func emptyState(unavailable: Bool) -> some View {
        VStack(spacing: 0) {
            ZStack {
                Circle().fill(unavailable ? oink.badSoft : oink.accentSoft)
                if unavailable {
                    Lucide(name: "shield-alert", size: 28, color: oink.badInk)
                } else {
                    Lucide(name: "radar", size: 30, color: oink.accentInk)
                    RadarPing()
                }
            }
            .frame(width: 72, height: 72)
            Text(unavailable ? "无法搜索附近的设备" : "正在搜索附近的设备")
                .font(.oink(16, .semibold)).foregroundStyle(oink.ink)
                .padding(.top, 16)
            Text(unavailable ? "Oink 需要「本地网络」权限才能发现同一网络中的设备。" : "请确认对方已打开 Oink，并和本机连接到同一个网络。")
                .font(.oink(13)).foregroundStyle(oink.muted)
                .multilineTextAlignment(.center).lineSpacing(13 * 0.55 - 4)
                .frame(maxWidth: 280)
                .padding(.top, 6)
            if unavailable {
                Button("前往设置") {
                    if let url = URL(string: UIApplication.openSettingsURLString) { UIApplication.shared.open(url) }
                }
                .buttonStyle(CapsuleButtonStyle(kind: .primary))
                .padding(.top, 18)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, 16)
        .padding(.top, 28).padding(.bottom, 4)
    }

}

/// Expanding ring around the radar (2 s loop).
private struct RadarPing: View {
    @Environment(\.oink) private var oink
    var body: some View {
        TimelineView(.animation) { clock in
            let p = clock.date.timeIntervalSinceReferenceDate.truncatingRemainder(dividingBy: 2) / 2
            let e = 1 - pow(1 - p, 3)
            Circle().strokeBorder(oink.accent, lineWidth: 2)
                .scaleEffect(0.85 + 0.75 * e)
                .opacity(0.5 * (1 - e))
        }
        .allowsHitTesting(false)
    }
}

/// Add-page row (`.ad-row`): logo, 17/650 name, 13 sub line, trailing control.
struct PeerRow<Trailing: View>: View {
    @Environment(\.oink) private var oink
    let platform: DevicePlatform
    let name: String
    let sub: String
    var disabled = false
    var minHeight: CGFloat = 76
    @ViewBuilder var trailing: () -> Trailing
    var body: some View {
        HStack(spacing: 14) {
            PlatformLogo(platform: platform).opacity(disabled ? 0.45 : 1)
            VStack(alignment: .leading, spacing: 3) {
                Text(name).font(.oink(17, .semibold)).kerning(-0.2)
                    .foregroundStyle(disabled ? oink.faint : oink.ink)
                    .multilineTextAlignment(.leading)
                Text(sub).font(.oink(13).monospacedDigit())
                    .foregroundStyle(disabled ? oink.faint : oink.muted)
                    .multilineTextAlignment(.leading)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            trailing()
        }
        .padding(.vertical, 12)
        .frame(minHeight: minHeight)
        .contentShape(Rectangle())
    }
}

// MARK: - Sheets

/// Hosts the manual-add and pairing sheets on the native sheet glass. Their inputs and buttons
/// use the mockup's tokens (`settings-pairing.css`), not the Control Center glass of the device
/// sheet: inputs `--c-elev`, primary capsule `--accent-soft`, grey capsule `--c-chip`.
struct PairingSheetHost: View {
    @EnvironmentObject private var pairing: PairingFlow
    let sheet: PairingSheetKind
    var body: some View {
        switch sheet {
        case .manual: ManualSheet()
        case .pair(let id): PairSheet(id: id)
        }
    }
}

/// 手动添加 sheet (mockup `ManualSheet`).
private struct ManualSheet: View {
    @EnvironmentObject private var core: CoreSession
    @EnvironmentObject private var pairing: PairingFlow
    @Environment(\.oink) private var oink
    @FocusState private var focused: PairingFlow.ManualForm.Field?

    var body: some View {
        let form = pairing.manual
        VStack(alignment: .leading, spacing: 0) {
            Text("手动添加").font(.oink(20, .semibold)).foregroundStyle(oink.ink)
            Text("输入对方的 IP 地址。对方的地址显示在它「添加设备」页的底部。")
                .font(.oink(13)).foregroundStyle(oink.muted).lineSpacing(3)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.top, 6)
            HStack(alignment: .top, spacing: 10) {
                field("IP 地址", text: Binding(get: { pairing.manual.ip }, set: { value in
                    let clean = String(value.filter { $0.isNumber || $0 == "." || $0 == ":" }.prefix(21))
                    if clean != pairing.manual.ip, pairing.manual.errorField == .ip { pairing.manual.error = nil; pairing.manual.errorField = nil }
                    if clean != pairing.manual.ip, core.foundPeer != nil { core.foundPeer = nil }
                    pairing.manual.ip = clean
                }), placeholder: "192.168.1.20", invalid: form.errorField == .ip, which: .ip)
                field("端口", text: Binding(get: { pairing.manual.port }, set: { value in
                    if pairing.manual.errorField == .port { pairing.manual.error = nil; pairing.manual.errorField = nil }
                    pairing.manual.port = String(value.filter(\.isNumber).prefix(5))
                }), placeholder: "47470", invalid: form.errorField == .port, which: .port)
                .frame(width: 96)
            }
            .modifier(ShakeEffect(travel: CGFloat(form.shakes)))
            .animation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.52), value: form.shakes)
            .padding(.top, 18)
            if let error = form.error {
                HStack(alignment: .top, spacing: 6) {
                    Lucide(name: "alert-circle", size: 16, color: oink.badInk).padding(.top, 1)
                    Text(error).font(.oink(13)).foregroundStyle(oink.badInk)
                }
                .padding(.leading, 4).padding(.top, 10)
                .transition(.opacity)
            }
            if let found = core.foundPeer {
                VStack(spacing: 0) {
                    PeerRow(platform: DevicePlatform(os: found.peer.os, name: found.peer.name), name: found.peer.name,
                            sub: [found.peer.address, core.displayFingerprint(found.peer.device)]
                                .compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · "),
                            minHeight: 70) { EmptyView() }
                    Button(action: pairing.pairFound) {
                        HStack(spacing: 6) { Lucide(name: "plus", size: 17, stroke: "2_4", color: oink.accentInk); Text("配对") }
                    }
                    .buttonStyle(CapsuleButtonStyle(kind: .primary, fullWidth: true))
                    .padding(.top, 6)
                }
                .padding(.horizontal, 16).padding(.top, 2).padding(.bottom, 16)
                .background(oink.elev, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 20, style: .continuous).strokeBorder(oink.line, lineWidth: 1))
                .padding(.top, 16)
            } else {
                Button(action: pairing.searchManual) {
                    HStack(spacing: 6) {
                        if form.searching { OinkSpinner(size: 18); Text("正在查找…") } else { Text("查找") }
                    }
                }
                .buttonStyle(CapsuleButtonStyle(kind: .primary, fullWidth: true))
                .disabled(form.searching)
                .padding(.top, 18)
            }
        }
        .fittedSheet()
        .onAppear { if DesignScene.current == nil { focused = .ip } }
    }

    private func field(_ title: String, text: Binding<String>, placeholder: String, invalid: Bool, which: PairingFlow.ManualForm.Field) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.oink(12.5, .semibold)).foregroundStyle(oink.text2).padding(.leading, 4).frame(height: 18)
            TextField("", text: text, prompt: Text(placeholder).foregroundStyle(oink.faint))
                .font(.oink(16).monospacedDigit())
                .foregroundStyle(oink.ink)
                .tint(oink.accent)
                .keyboardType(which == .ip ? .decimalPad : .numberPad)
                .focused($focused, equals: which)
                .padding(.horizontal, 14)
                .frame(height: 48)
                .background(oink.elev, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
                .contentShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
                .overlay {
                    let ring: Color = invalid ? oink.bad : focused == which ? oink.accent : oink.track
                    RoundedRectangle(cornerRadius: 14, style: .continuous).strokeBorder(ring, lineWidth: 1)
                }
                .background {
                    if invalid || focused == which {
                        RoundedRectangle(cornerRadius: 14, style: .continuous)
                            .stroke(invalid ? oink.badSoft : oink.accentSoft, lineWidth: 6)
                    }
                }
        }
    }
}

/// Modal pairing sheet (mockup `PairSheet`): no grabber, cannot be swiped away. Closes via
/// 取消配对 / 关闭, or by itself after success.
private struct PairSheet: View {
    @EnvironmentObject private var core: CoreSession
    @EnvironmentObject private var pairing: PairingFlow
    @Environment(\.oink) private var oink
    let id: String

    private enum Phase: Equatable {
        case connecting, code, entry, verifying, wrong, success, failed(String)
        var key: String { if case .failed = self { return "failed" }; return "\(self)" }
    }

    var body: some View {
        VStack(spacing: 0) {
            if let pair = pairing.session(id) {
                content(pair)
            } else {
                Spacer()
            }
        }
        .fittedSheet(dragIndicator: .hidden)
        .interactiveDismissDisabled(true)
        // `initial`: a sheet opened for a session the snapshot no longer has closes at once
        // instead of staying empty with no button.
        .onChange(of: pairing.session(id) == nil, initial: true) { _, gone in if gone { pairing.sessionEnded(id) } }
    }

    private func phase(_ pair: PairView) -> Phase {
        if let terminal = pair.terminal { return .failed(CoreSession.pairProblemText(terminal)) }
        if pair.feedback == .correct { return .success }
        if pair.phase == 0 { return .connecting }
        if pair.direction == .outgoing { return pairing.shownCodes[pair.id] == nil ? .connecting : .code }
        if pairing.verifying.contains(pair.id) { return .verifying }
        if pairing.wrongShown.contains(pair.id) { return .wrong }
        return .entry
    }

    @ViewBuilder private func content(_ pair: PairView) -> some View {
        let phase = phase(pair)
        let failed: Bool = { if case .failed = phase { return true } else { return false } }()
        HStack(spacing: 12) {
            PlatformLogo(platform: DevicePlatform(os: pair.peer.os, name: pair.peer.name))
                .task(id: pair.id) { if pair.direction == .outgoing { pairing.loadCode(pair) } }
            VStack(alignment: .leading, spacing: 2) {
                Text("与 \(pair.peer.name) 配对").font(.oink(19, .semibold)).foregroundStyle(oink.ink)
                Text(subtitle(pair)).font(.oink(11.5).monospacedDigit()).foregroundStyle(oink.faint)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(minHeight: 44)
        .padding(.bottom, 4)
        ZStack { body(pair, phase).id(phase.key).transition(.opacity.combined(with: .offset(y: 8))) }
            .animation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.24), value: phase.key)
            .frame(maxWidth: .infinity, minHeight: 186)
            .padding(.top, 14).padding(.bottom, 10)
            .frame(height: 186)
        if !failed {
            VStack(spacing: 0) {
                Hairline()
                HStack(alignment: .top, spacing: 8) {
                    Lucide(name: "shield-alert", size: 14, color: oink.muted).padding(.top, 2)
                    Text("配对后，对方可以和本机共享剪贴板、文件和音频。请确认你信任此设备。")
                        .font(.oink(12)).foregroundStyle(oink.muted).lineSpacing(2.6)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .padding(.horizontal, 4).padding(.top, 14)
            }
        }
        Button(failed ? "关闭" : "取消配对") { pairing.cancel(pair) }
            .buttonStyle(CapsuleButtonStyle(kind: .soft, fullWidth: true))
            .disabled(phase == .success)
            .padding(.top, 18)
    }

    private func subtitle(_ pair: PairView) -> String {
        [core.displayAddress(pair.peer.device), core.displayFingerprint(pair.peer.device)]
            .compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · ")
    }

    @ViewBuilder private func body(_ pair: PairView, _ phase: Phase) -> some View {
        switch phase {
        case .connecting:
            VStack(spacing: 14) {
                OinkSpinner(size: 32)
                Text("正在连接…").font(.oink(13.5)).foregroundStyle(oink.muted).frame(minHeight: 22)
            }
            .onAppear { if pair.direction == .outgoing { pairing.loadCode(pair) } }
        case .failed(let reason):
            VStack(spacing: 14) {
                Lucide(name: "x", size: 24, stroke: "2_6", color: oink.badInk)
                    .frame(width: 52, height: 52).background(oink.badSoft, in: Circle())
                Text("配对失败").font(.oink(17, .semibold)).foregroundStyle(oink.ink)
                Text(reason).font(.oink(13.5)).foregroundStyle(oink.text2).multilineTextAlignment(.center).padding(.top, -8)
            }
            .onAppear { Haptics.error() }
        case .code, .success where pair.direction == .outgoing:
            VStack(spacing: 14) {
                Text("在 \(pair.peer.name) 上输入这个配对码").font(.oink(14)).foregroundStyle(oink.text2)
                CodeCells(value: .constant(pairing.shownCodes[pair.id] ?? ""), success: phase == .success, showOnly: true)
                status(phase == .success)
            }
        default:
            VStack(spacing: 14) {
                Text("输入 \(pair.peer.name) 屏幕上显示的配对码").font(.oink(14)).foregroundStyle(oink.text2)
                CodeCells(value: Binding(get: { pairing.input[pair.id] ?? "" }, set: { pairing.setInput(pair.id, $0) }),
                          invalid: phase == .wrong, disabled: phase == .verifying || phase == .success,
                          // Spec「成功」: the digits turn green on both sides.
                          success: phase == .success,
                          cellFill: oink.elev,
                          onComplete: { pairing.submit(pair, code: $0) })
                    .modifier(ShakeEffect(travel: phase == .wrong ? 1 : 0))
                    .animation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.52), value: phase == .wrong)
                Group {
                    switch phase {
                    case .verifying:
                        HStack(spacing: 6) { OinkSpinner(size: 16); Text("正在验证…") }.foregroundStyle(oink.muted)
                    case .wrong:
                        HStack(spacing: 6) {
                            Lucide(name: "alert-circle", size: 16, color: oink.badInk)
                            Text("配对码不正确，还可以再试 \(pair.attemptsLeft) 次")
                        }.foregroundStyle(oink.badInk)
                    case .success: status(true)
                    default:
                        Text(pair.attemptsLeft < 3 ? "还可以再试 \(pair.attemptsLeft) 次" : " ").foregroundStyle(oink.muted)
                    }
                }
                .font(.oink(13.5))
                .frame(minHeight: 22)
            }
        }
        Color.clear.frame(width: 0, height: 0)
            .onChange(of: pair.feedback == .correct, initial: true) { _, ok in if ok { pairing.succeeded(pair) } }
    }

    @ViewBuilder private func status(_ success: Bool) -> some View {
        if success {
            HStack(spacing: 6) {
                Lucide(name: "check", size: 17, stroke: "2_6", color: oink.goodInk)
                Text("配对成功").font(.oink(13.5, .semibold))
            }
            .foregroundStyle(oink.goodInk).frame(minHeight: 22)
        } else {
            HStack(spacing: 6) { OinkDots(); Text("等待对方输入").font(.oink(13.5)) }
                .foregroundStyle(oink.muted).frame(minHeight: 22)
        }
    }
}
