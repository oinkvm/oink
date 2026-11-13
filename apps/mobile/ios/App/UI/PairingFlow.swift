import Combine
import SwiftUI

enum PairingSheetKind: Identifiable, Equatable {
    case manual
    case pair(String)
    var id: String { switch self { case .manual: "manual"; case .pair(let id): "pair-\(id)" } }
}

/// Pairing state above the add page (mockup `usePairingDemo`): the open sheet, the manual
/// add form, and the local UI state of each pairing session. Sessions themselves come from
/// the core's pairing snapshot.
@MainActor
final class PairingFlow: ObservableObject {
    struct ManualForm: Equatable {
        enum Field { case ip, port }
        var ip = ""
        var port = ""
        var error: String?
        var errorField: Field?
        var searching = false
        var shakes = 0
    }

    @Published var sheet: PairingSheetKind?
    @Published var manual = ManualForm()
    /// Digits typed into each incoming session.
    @Published var input: [String: String] = [:]
    @Published var verifying: Set<String> = []
    /// A wrong code stays red for 650 ms before the cells clear.
    @Published var wrongShown: Set<String> = []
    @Published var shownCodes: [String: String] = [:]
    @Published var showIPs = false
    /// Bumped when a success sheet closes: the app then shows the devices page (user 2026-11-11).
    @Published private(set) var pairedReturns = 0
    private weak var core: CoreSession?
    private weak var toasts: ToastCenter?
    private var finishing: Set<String> = []
    private var snapshotObservation: AnyCancellable?
    /// The session as it was when its code was submitted.
    private var submitted: [String: PairView] = [:]

    func attach(core: CoreSession, toasts: ToastCenter) {
        self.core = core
        self.toasts = toasts
        snapshotObservation = core.$pairingSnapshot.sink { [weak self] snapshot in
            // `@Published` emits before the property changes; read the value.
            Task { @MainActor in self?.codeChecked(snapshot) }
        }
    }

    /// A submitted code stays "verifying" until the peer's verdict arrives in
    /// a later snapshot; only then does a wrong code turn red and clear
    /// (review L15). Submitting returns as soon as the attempt is queued.
    private func codeChecked(_ snapshot: PairingSnapshot?) {
        for (id, before) in submitted {
            switch PairingCodeCheck.outcome(submitted: before, current: snapshot?.sessions.first { $0.id == id }) {
            case .pending: continue
            case .wrong:
                finishVerifying(id)
                showWrong(id)
            case .settled:
                finishVerifying(id)
            }
        }
    }

    private func finishVerifying(_ id: String) {
        submitted[id] = nil
        verifying.remove(id)
    }

    var requests: [PendingRequest] { core?.pairingSnapshot?.requests ?? [] }

    func session(_ id: String) -> PairView? { core?.pairingSnapshot?.sessions.first { $0.id == id } }

    private var rows: [PairingInbox.Row] {
        (core?.pairingSnapshot?.sessions ?? []).map {
            PairingInbox.Row(id: $0.id, incoming: $0.direction == .incoming, phase: $0.phase,
                             ended: $0.terminal != nil, device: $0.peer.device)
        }
    }

    /// Opens the sheet of the session the core just started or accepted.
    private func open(started id: String?) {
        if let id = PairingInbox.startedSession(rows, started: id) { sheet = .pair(id) }
    }

    // MARK: Actions

    func connect(_ row: NearbyDevice) {
        guard row.compatible, let core else { return }
        Haptics.selection()
        if let running = PairingInbox.runningSession(rows, device: row.device) {
            sheet = .pair(running); return
        }
        Task { @MainActor in
            do {
                let started = try await core.startPairing(.nearbyDevice(device: row.device))
                open(started: started)
            } catch { toasts?.show(error.oinkMessage) }
        }
    }

    func accept(_ request: PendingRequest) {
        guard let core else { return }
        Haptics.selection()
        Task { @MainActor in
            do {
                try await core.acceptPairing(request)
                // An accepted request continues as the session with the request's key.
                open(started: request.id)
            } catch { toasts?.show(error.oinkMessage) }
        }
    }

    func reject(_ request: PendingRequest) {
        guard let core else { return }
        Haptics.selection()
        Task { @MainActor in
            do {
                try await core.declinePairing(request)
                toasts?.show("已拒绝 \(request.peer.name) 的配对请求")
            } catch { toasts?.show(error.oinkMessage) }
        }
    }

    func setInput(_ id: String, _ value: String) {
        input[id] = value
        wrongShown.remove(id)
    }

    func submit(_ pair: PairView, code: String) {
        guard let core else { return }
        verifying.insert(pair.id)
        submitted[pair.id] = pair
        Task { @MainActor in
            do {
                try await core.submitPairingCode(pair, code: code)
                codeChecked(core.pairingSnapshot)
            } catch {
                finishVerifying(pair.id)
                toasts?.show(error.oinkMessage)
            }
        }
    }

    private func showWrong(_ id: String) {
        Haptics.error()
        wrongShown.insert(id)
        Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(650))
            guard wrongShown.contains(id) else { return }
            wrongShown.remove(id)
            input[id] = ""
        }
    }

    func loadCode(_ pair: PairView) {
        guard shownCodes[pair.id] == nil, let core else { return }
        Task { @MainActor in
            if let code = try? await core.pairingCode(for: pair) { shownCodes[pair.id] = code }
        }
    }

    /// 取消配对 / 关闭. A failed session only closes; a live one is cancelled. Neither shows a
    /// toast (user 2026-11-11, same as Android); only a failed cancel reports its error.
    func cancel(_ pair: PairView) {
        guard let core else { return }
        Haptics.selection()
        let failed = pair.terminal != nil
        sheet = nil
        Task { @MainActor in
            if !failed {
                do { try await core.cancelPairing(pair) }
                catch { toasts?.show(error.oinkMessage) }
            }
        }
    }

    /// Success: 900 ms later the sheet closes and the app returns to the devices page, whichever
    /// side started the pairing; no device sheet opens (user 2026-11-11). The「已与 X 配对」toast
    /// follows the core's PairingCompleted event (no undo, decision 38). The close is scheduled
    /// every time a success sheet shows, so a success sheet never stays open with its disabled
    /// button.
    func succeeded(_ pair: PairView) {
        if finishing.insert(pair.id).inserted { Haptics.success() }
        #if DEBUG
        // The static success states stay open for screenshots; pair-complete-* run the close.
        if DesignScene.current != nil && !DesignScene.hasPrefix("pair-complete") { return }
        #endif
        Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(900))
            if sheet == .pair(pair.id) {
                sheet = nil
                pairedReturns += 1
            }
        }
    }

    /// The session left the core's snapshot (committed or cleared elsewhere):
    /// nothing is left to show, so its sheet closes.
    func sessionEnded(_ id: String) {
        #if DEBUG
        if DesignScene.current != nil { return }
        #endif
        if sheet == .pair(id) { sheet = nil }
    }

    func pairingCompleted(_ completed: IOSPairingCompleted) {
        toasts?.show("已与 \(completed.peerName) 配对")
    }

    // MARK: Manual add

    func openManual() {
        Haptics.selection()
        manual = ManualForm()
        core?.foundPeer = nil
        core?.manualAddProblem = nil
        sheet = .manual
    }

    func searchManual() {
        guard let core, !manual.searching else { return }
        let parts = manual.ip.trimmingCharacters(in: .whitespaces).split(separator: ":", omittingEmptySubsequences: false).map(String.init)
        let ip = parts.first ?? ""
        let inlinePort = parts.count > 1 ? parts[1] : nil
        func fail(_ text: String, _ field: ManualForm.Field) {
            Haptics.error()
            manual.error = text; manual.errorField = field; manual.shakes += 1
        }
        if ip.isEmpty { return fail("请输入 IP 地址", .ip) }
        let octets = ip.split(separator: ".", omittingEmptySubsequences: false)
        let ipv4 = octets.count == 4 && octets.allSatisfy { !$0.isEmpty && $0.count <= 3 && $0.allSatisfy(\.isNumber) && Int($0)! <= 255 }
        if parts.count > 2 || !ipv4 { return fail("请输入有效的 IPv4 地址，例如 192.0.2.20", .ip) }
        let portText = inlinePort ?? manual.port.trimmingCharacters(in: .whitespaces)
        let port = portText.isEmpty ? 47470 : Int(portText) ?? 0
        if inlinePort == "" || !(1...65535).contains(port) { return fail("端口应在 1–65535 之间", .port) }
        Haptics.selection()
        manual.error = nil; manual.errorField = nil; manual.searching = true
        Task { @MainActor in
            do { try await core.queryAddress("\(ip):\(port)") }
            catch { manual.searching = false; manual.error = error.oinkMessage; manual.errorField = nil }
        }
    }

    /// Called when the core answers the address query.
    func manualQueryFinished(found: Bool, problem: String?) {
        guard manual.searching else { return }
        manual.searching = false
        if !found, let problem { manual.error = problem; manual.errorField = nil }
    }

    func pairFound() {
        guard let core, let found = core.foundPeer else { return }
        Haptics.selection()
        sheet = nil
        Task { @MainActor in
            do {
                let started = try await core.startPairing(.preparedQuery(queryId: found.queryId, generation: found.generation))
                open(started: started)
            } catch { toasts?.show(error.oinkMessage) }
        }
    }

    #if DEBUG
    /// Opens the sheet / form / toast of a design state (`-oink-ui-scene`).
    func applyDesignScene() {
        guard let scene = DesignScene.current else { return }
        switch scene {
        case "manual": manual = ManualForm(); sheet = .manual
        case "manual-invalid":
            manual = ManualForm(ip: "192.168.1", error: "请输入有效的 IPv4 地址，例如 192.0.2.20", errorField: .ip); sheet = .manual
        case "manual-searching": manual = ManualForm(ip: "192.0.2.24", searching: true); sheet = .manual
        case "manual-found": manual = ManualForm(ip: "192.0.2.24"); sheet = .manual
        case "add-info", "add-info-all": showIPs = true
        case "pair-paired-toast": toasts?.show("已与 Studio-Mac 配对", duration: 3600)
        case "manual-notfound":
            manual = ManualForm(ip: "192.0.2.250", error: CoreSession.pairProblemText(PairProblem(code: 7, stage: 0, detail: "", network: .notOink)))
            sheet = .manual
        case "manual-timeout":
            manual = ManualForm(ip: "192.0.2.9", error: CoreSession.pairProblemText(PairProblem(code: 6, stage: 0, detail: "", network: .timeout)))
            sheet = .manual
        default:
            guard scene.hasPrefix("pair-") else { return }
            let typed = ["pair-entry-typing": "2468", "pair-verifying": "246810", "pair-wrong": "111111", "pair-success": "246810",
                         "pair-complete-in": "246810"]
            input["fixture-pair"] = typed[scene] ?? ""
            // The shown code of the outgoing states (the core has no fixture session to ask).
            if ["pair-code", "pair-success-out", "pair-complete-out"].contains(scene) { shownCodes["fixture-pair"] = "246810" }
            if scene == "pair-verifying" { verifying.insert("fixture-pair") }
            if scene == "pair-wrong" { wrongShown.insert("fixture-pair") }
            sheet = .pair("fixture-pair")
        }
    }
    #endif
}
