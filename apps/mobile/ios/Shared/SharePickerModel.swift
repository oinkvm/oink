import Foundation

/// The share / open-with device picker (native-rewrite.md「分享 / 打开方式先选设备」,
/// review 2026-11-11). It opens with the devices last seen online (grey「上次在线」)
/// while the session's first round runs, and follows the live online list. No selected
/// state and no confirm button: a tap on an online device starts the transfer; a tap on
/// a device not confirmed yet waits for it (system spinner, row disabled), then starts,
/// or says「X 不在线」and drops it from the list.
@MainActor
final class SharePickerModel: ObservableObject {
    struct Row: Identifiable, Equatable {
        var device: SendTarget
        /// Seen online in this session (「在线」); false: from the cache (「上次在线」).
        var confirmed: Bool
        var id: String { device.id }
    }

    /// The light hint of a device found offline; `id` tells two equal texts apart.
    struct Notice: Equatable {
        var id = UUID()
        var text: String
    }

    @Published private(set) var rows: [Row] = []
    /// The first round runs (「正在查找设备…」).
    @Published private(set) var searching = true
    /// The row being confirmed.
    @Published private(set) var connecting: String?
    @Published private(set) var notice: Notice?

    private let cached: [SendTarget]
    private var online: [SendTarget] = []
    private var removed: Set<String> = []
    /// The group's only other device, sent to without a choice (`showOnly`).
    private var only: SendTarget?
    private let chinese: Bool

    init(cached: [SendTarget], chinese: Bool) {
        self.cached = cached
        self.chinese = chinese
        rebuild()
    }

    /// No device online once the first round ended.
    var isEmpty: Bool { !searching && rows.isEmpty }
    var computers: [Row] { rows.filter(\.device.isComputer) }
    var phones: [Row] { rows.filter { !$0.device.isComputer } }

    /// The session's online devices (in its order) and whether its first round ended.
    /// While it runs, the cached devices not seen yet stay (unconfirmed); after it,
    /// only the online ones remain.
    func update(online: [SendTarget], roundFinished: Bool) {
        self.online = online
        if roundFinished { searching = false }
        rebuild()
    }

    /// The group has one other device: the share goes to it without a choice, and the page
    /// under the send card lists that device alone (user 2026-11-12), 「在线」while the
    /// session sees it online and「上次在线」otherwise (the core waits for it then).
    func showOnly(_ device: SendTarget) {
        only = device
        rebuild()
    }

    /// A tap. Returns the device to send to, or nil (busy, or it was not online).
    /// `confirm` decides a cached device: true once it is online, false when it is not.
    func pick(_ id: String, confirm: (SendTarget) async -> Bool) async -> SendTarget? {
        guard connecting == nil, let row = rows.first(where: { $0.id == id }) else { return nil }
        if row.confirmed { return row.device }
        connecting = id
        let reachable = await confirm(row.device)
        connecting = nil
        if reachable { return row.device }
        removed.insert(id)
        notice = Notice(text: chinese ? "\(row.device.name) 不在线" : "\(row.device.name) is offline")
        rebuild()
        return nil
    }

    /// While searching the cached devices keep their places (a confirmed one turns
    /// 「在线」) and newly seen ones follow; afterwards the online list as it is.
    private func rebuild() {
        let live = online.filter { !removed.contains($0.id) }
        var next: [Row]
        if let only {
            let seen = live.first { $0.id == only.id }
            next = [Row(device: seen ?? only, confirmed: seen != nil)]
        } else if searching {
            let liveByID = Dictionary(live.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
            let cachedIDs = Set(cached.map(\.id))
            next = cached.filter { !removed.contains($0.id) }.map { Row(device: liveByID[$0.id] ?? $0, confirmed: liveByID[$0.id] != nil) }
            next += live.filter { !cachedIDs.contains($0.id) }.map { Row(device: $0, confirmed: true) }
        } else {
            next = live.map { Row(device: $0, confirmed: true) }
        }
        if next != rows { rows = next }
    }
}
