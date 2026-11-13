import Combine
import Foundation

/// Records whose stored state changed. TransferModel reloads exactly these
/// (`CoreSession.transferMessage`) instead of reloading the whole page.
struct IOSTransferChange: Equatable {
    var sequence: UInt64 = 0
    var recordIDs: [Int64] = []
}

struct CoreTransferPage {
    var messages: [TransferMessage]
    var nextCreatedMs: Int64?
    var nextID: Int64?
}

/// What the transfer history reads from the running session (CoreSession).
@MainActor
protocol TransferHistorySource: AnyObject {
    /// Records that changed (`CoreSession.transferChange`); sends its current value first.
    var transferChanges: AnyPublisher<IOSTransferChange, Never> { get }
    /// A new session started (`CoreSession.sessionRevision`); sends its current value first.
    var sessionRevisions: AnyPublisher<Int, Never> { get }
    var hasLiveSession: Bool { get }
    /// DEBUG design states set the progress of their fixture records here.
    var recordProgress: [Int64: TransferOperationProgress] { get set }
    func sharedHistoryBoundary() -> Date?
    func sharedHistoryHasMore() -> Bool
    func loadOlderSharedHistory() async throws
    func transferPage(createdMs: Int64?, id: Int64?, limit: UInt32) throws -> CoreTransferPage
    func transferMessage(_ id: Int64) throws -> TransferMessage?
    func deleteTransferMessage(_ message: TransferMessage) throws
    func requestTransferThumbnail(_ message: TransferMessage)
    func startTransferDownload(_ message: TransferMessage, open: Bool) throws
    /// The rich text kept with a text message (`MobileSession.transfer_rich_text`);
    /// nil when it has none on this phone (yet).
    func transferRichText(_ message: TransferMessage) -> TransferRichText?
}

extension TransferHistorySource {
    func sharedHistoryBoundary() -> Date? { nil }
    func sharedHistoryHasMore() -> Bool { false }
    func loadOlderSharedHistory() async throws {}
    func transferRichText(_ message: TransferMessage) -> TransferRichText? { nil }
}

/// Transfer page history. The Rust session's store is the only store: pages,
/// single-record reloads and deletes all go through CoreSession, so delete
/// and 清理缓存 act on the live download state.
@MainActor
final class TransferModel: ObservableObject {
    /// Newest first.
    @Published private(set) var messages: [TransferMessage] = []
    @Published private(set) var isLoading = false
    @Published var errorMessage = ""
    @Published private(set) var olderLoadFailed = false
    @Published private(set) var sharedBoundary: Date?
    var hasOlder: Bool { !reachedEnd || core.sharedHistoryHasMore() }
    private let core: any TransferHistorySource
    private var oldestCreatedMs: Int64?
    private var oldestID: Int64?
    private var reachedEnd = false
    private var subscriptions: Set<AnyCancellable> = []
    /// Parsed rich text by message id (richContent): read from the core and parsed once per
    /// record, not each time a row is built.
    private var richCache: [String: RichContent] = [:]
    #if DEBUG
    /// Design states' rich text by message id (transfer-rich-*, TransferRichFixture).
    private var fixtureRich: [String: TransferRichText] = [:]
    #endif

    init(core: any TransferHistorySource) {
        self.core = core
        // The model follows the session's changes itself, for as long as it lives.
        // The page used to forward them with `onReceive`, and SwiftUI may subscribe
        // that again when it updates the page: a change sent in the same main-queue
        // turn as anything the page shows (the progress a finished transfer clears)
        // was dropped with the old subscription, and the new one skips the current
        // value. The record then kept its old state (发送中, a receive at 0 %)
        // until the app was restarted.
        // Each change is handled after the session's current batch of events
        // (@Published sends before the property is set).
        core.transferChanges.dropFirst().receive(on: DispatchQueue.main)
            .sink { [weak self] change in MainActor.assumeIsolated { self?.apply(change) } }
            .store(in: &subscriptions)
        core.sessionRevisions.dropFirst().removeDuplicates().receive(on: DispatchQueue.main)
            .sink { [weak self] _ in MainActor.assumeIsolated { self?.reload() } }
            .store(in: &subscriptions)
        #if DEBUG
        // Before the first render, so the fixture never flashes the empty state.
        installUIFixtureIfRequested()
        #endif
    }

    #if DEBUG
    /// Deterministic conversation for the design-state screenshots
    /// (`-oink-ui-scene transfer…`, DEBUG only). Senders are the fixture devices
    /// of CoreSession; ids are numeric so per-record progress applies.
    func installUIFixtureIfRequested() {
        guard let scene = DesignScene.current, scene.hasPrefix("transfer") else { return }
        // A conversation of real size for the scroll measurement.
        if scene == "transfer-perf" {
            if messages.isEmpty { messages = Self.fixtureStatuses(TransferPerfFixture.messages()) }
            reachedEnd = true
            return
        }
        let start = Date().addingTimeInterval(-600)
        func message(_ n: Int, _ kind: TransferMessageKind, sender: String, name: String? = nil, text: String? = nil,
                     size: Int64? = nil, state: AttachmentState? = nil, proxy: String? = nil, mine: Bool = false,
                     operation: String? = nil) -> TransferMessage {
            TransferMessage(id: String(9000 + n), createdAt: start.addingTimeInterval(Double(n)), senderDeviceID: proxy ?? sender,
                            senderName: sender, offerID: nil, sourceBoot: nil, operationID: operation, itemID: nil, representationID: nil,
                            originSenderID: sender, proxyDeviceID: proxy, proxyName: proxy, kind: kind, text: text,
                            contentID: kind == .text ? nil : operation == nil ? "fixture-content-\(n)" : "fixture-batch-\(n)",
                            attachmentName: name, byteSize: size,
                            uti: nil, state: state, availability: "Ready", isMine: mine, delivery: mine ? .sent : nil)
        }
        if scene.hasPrefix("transfer-history") {
            messages = [message(2, .text, sender: "fixture-pc", text: "较新的共享记录"),
                        message(1, .text, sender: "self", text: "更早的本机记录", mine: true)]
            reachedEnd = scene == "transfer-history-boundary"
            olderLoadFailed = scene == "transfer-history-failed"
            if reachedEnd { sharedBoundary = start.addingTimeInterval(1.5) }
            return
        }
        if scene.hasPrefix("transfer-p2p") {
            messages = Self.fixtureStatuses(p2pFixture(scene, message).reversed())
            reachedEnd = true
            return
        }
        // Rich text bubbles (mockup scenes transfer-rich-*, user 2026-11-12).
        if scene.hasPrefix("transfer-rich") {
            let list = TransferRichFixture.messages(scene) { n, sender, text, mine in
                message(n, .text, sender: sender, text: text, mine: mine)
            }
            fixtureRich = Dictionary(uniqueKeysWithValues: list.compactMap { m in m.rich.map { (m.message.id, $0) } })
            richCache = [:]
            messages = Self.fixtureStatuses(list.map { $0.message }.reversed())
            reachedEnd = true
            return
        }
        // File cards, picture sizes and the 已发送 rule (native-rewrite.md「文件卡片」「图片尺寸」
        // 「发送状态行」, mockup scenes transfer-file-*, transfer-sent-latest, transfer-images).
        func progress(_ id: Int, _ value: UInt64, sending: Bool = false) {
            core.recordProgress[Int64(9000 + id)] = sending ? TransferOperationProgress(sent: value, total: 100)
                : TransferOperationProgress(received: value, total: 100)
        }
        func direct(_ m: TransferMessage, failure: String? = nil) -> TransferMessage {
            var m = m
            m.directPeerID = "fixture-android"
            m.directPeerName = "Android"
            if m.isMine, failure != nil { m.delivery = .failed }
            m.deliveryFailure = failure
            return m
        }
        func gone(_ m: TransferMessage, _ availability: String) -> TransferMessage {
            var m = m
            m.availability = availability
            m.sourceOnline = availability != "source_offline"
            return m
        }
        let fileScene: [TransferMessage]? = switch scene {
        case "transfer-file-received":
            [message(201, .file, sender: "fixture-windowsPc", name: "设计说明.pdf", size: 2_400_000, state: .placeholder),
             message(202, .file, sender: "fixture-windowsPc", name: "会议纪要.docx", size: 86_000, state: .downloading),
             message(203, .file, sender: "fixture-windowsPc", name: "排期表（11 月第二周，含节假日调整）.xlsx", size: 312_000, state: .stored),
             { var m = message(204, .folder, sender: "fixture-macMini", name: "项目素材", state: .placeholder); m.itemCount = 12; return m }()]
        case "transfer-file-problems":
            [direct(message(211, .file, sender: "fixture-android", name: "现场照片（原图）.zip", size: 486_000_000, state: .failed),
                    failure: "对方已断开，接收失败"),
             direct(message(212, .file, sender: "fixture-android", name: "合同扫描.pdf", size: 1_200_000, state: .cancelled)),
             gone(message(213, .file, sender: "fixture-macMini", name: "Oink-0.3.4.dmg", size: 48_200_000, state: .unavailable), "no_such_item"),
             gone(message(214, .file, sender: "fixture-desktop", name: "报价单 v3.docx", size: 240_000, state: .unavailable), "source_offline")]
        case "transfer-file-sent":
            [{ var m = direct(message(221, .file, sender: "self", name: "现场照片（原图）.zip", size: 486_000_000, state: .stored, mine: true))
               m.delivery = .sending; return m }(),
             message(222, .file, sender: "self", name: "合同扫描.pdf", size: 1_200_000, state: .stored, mine: true),
             message(223, .file, sender: "self", name: "2026 年第四季度产品路线图与里程碑（评审修订版，含附录）.docx", size: 1_600_000,
                     state: .stored, mine: true),
             { var m = direct(message(224, .folder, sender: "self", name: "现场照片", state: .stored, mine: true), failure: "对方已断开，发送失败")
               m.itemCount = 24; return m }()]
        case "transfer-sent-latest":
            [message(230, .text, sender: "fixture-windowsPc", text: "资料都发我一下。"),
             message(231, .file, sender: "self", name: "合同扫描.pdf", size: 1_200_000, state: .stored, mine: true),
             message(232, .image, sender: "self", name: "白板照片.jpg", size: 1_600_000, state: .stored, mine: true),
             { var m = direct(message(233, .folder, sender: "self", name: "现场照片", state: .stored, mine: true), failure: "对方已断开，发送失败")
               m.itemCount = 24; return m }(),
             message(234, .file, sender: "self", name: "排期表.xlsx", size: 312_000, state: .stored, mine: true)]
        case "transfer-images":
            // Received (downloaded), own (a 4 : 3 photo at its own proportions, no longer
            // cropped to 260 : 146), received (downloading).
            [message(241, .image, sender: "fixture-macMini", name: "桌面预览.png", size: 1_800_000, state: .stored),
             { var m = message(242, .image, sender: "self", name: "白板照片.jpg", size: 1_600_000, state: .stored, mine: true)
               m.contentID = "fixture-batch-21"; m.imageAspect = 4.0 / 3; return m }(),
             message(243, .image, sender: "fixture-windowsPc", name: "界面截图.png", size: 2_100_000, state: .downloading)]
        case "transfer-file-batch-received":
            // An expanded received batch: every download state, and a folder.
            [message(250, .text, sender: "fixture-windowsPc", text: "这几个文件你看一下。"),
             message(251, .file, sender: "fixture-windowsPc", name: "Q4 设计评审.pdf", size: 2_400_000, state: .placeholder, operation: "fixture-op-x"),
             message(252, .file, sender: "fixture-windowsPc", name: "会议纪要.docx", size: 86_000, state: .downloading, operation: "fixture-op-x"),
             message(253, .file, sender: "fixture-windowsPc", name: "排期表.xlsx", size: 312_000, state: .stored, operation: "fixture-op-x"),
             message(254, .file, sender: "fixture-windowsPc", name: "图标资源（全部尺寸）.zip", size: 18_600_000, state: .failed, operation: "fixture-op-x"),
             { var m = message(255, .folder, sender: "fixture-windowsPc", name: "项目素材", state: .placeholder, operation: "fixture-op-x")
               m.itemCount = 12; return m }()]
        default: nil
        }
        if let fileScene {
            progress(202, 45)
            progress(221, 45, sending: true)
            progress(243, 40)
            progress(252, 45)
            messages = Self.fixtureStatuses(fileScene.reversed())
            reachedEnd = true
            return
        }
        // Batches (stacked shares) and long texts (folding, 选择): the transfer-stack
        // design states, with the prototype's conversation (TransferPage.jsx DEMO_TAIL).
        if scene.hasPrefix("transfer-stack") || scene.hasPrefix("transfer-long") || scene.hasPrefix("transfer-select")
            || scene.hasPrefix("transfer-secret") {
            let longReceived = ["0.4.0 移动端联调记录（周一下午，参会：Alex、Windows PC、Mac mini）",
                                "1. 传输页：长文本默认只显示前 10 行，第 8–10 行渐隐，点「查看更多」每次多显示 20 行；长按复制仍然复制全文。",
                                "2. 多文件聚合：一次发送或接收的一批文件显示为一叠，左右划动切换最上面那张，点「展开」恢复成普通消息。",
                                "3. 展开后，第一个完整可见的项旁边显示「收起」，滚动时隐藏。"]
                + (4...20).map { "\($0). 联调事项第 \($0) 条：按设计稿逐项核对，结果记在共享文档里。" }
                + ["下周一再对一次，有问题直接在群里说。谢谢大家。"]
            let longSelf = ["收到，我这边的检查结果：",
                            "长文本折叠：iOS 和 Android 都已经按 10 行实现，渐隐范围是第 8 到第 10 行。",
                            "「查看更多」放在气泡里文字下方，水平居中。",
                            "自己发的气泡用主题色底，渐隐用透明度蒙版，同样适用。",
                            "长按复制的是全文，不是可见部分。",
                            "滚出一屏之后回来会恢复折叠，恢复时列表不跳动。",
                            "深色模式也检查过了，横屏和大折叠内屏还没看，明天补上截图。"]
            var list = [message(1, .text, sender: "fixture-windowsPc", text: "这版界面已经整理好了，发给你看看。"),
                        message(2, .text, sender: "self", text: "收到，我在手机上确认一下。", mine: true),
                        message(3, .image, sender: "fixture-macMini", name: "桌面预览.png", size: 1_800_000, state: .stored),
                        message(10, .text, sender: "fixture-windowsPc", text: longReceived.joined(separator: "\n")),
                        message(11, .text, sender: "self", text: longSelf.joined(separator: "\n"), mine: true)]
            let received = ["客厅 · 早上.jpg", "窗边.jpg", "午睡.jpg", "夜景.jpg", "沙丘.jpg", "花.jpg", "全景.jpg", "屏幕截图.png"]
            list += received.enumerated().map { i, name in
                message(20 + i, .image, sender: "fixture-macMini", name: name, size: 1_200_000, state: .stored, operation: "fixture-op-a")
            }
            let sent: [(TransferMessageKind, String, Int64?, Int?)] = [
                (.file, "Q4 设计评审.pdf", 2_400_000, nil), (.image, "白板照片.jpg", 1_600_000, nil),
                (.file, "图标资源（全部尺寸）.zip", 18_600_000, nil), (.folder, "现场照片", nil, 24),
                (.file, "排期表.xlsx", 312_000, nil), (.file, "会议纪要.docx", 86_000, nil)]
            list += sent.enumerated().map { i, item -> TransferMessage in
                var m = message(30 + i, item.0, sender: "self", name: item.1, size: item.2, state: .stored, mine: true,
                                operation: "fixture-op-b")
                m.itemCount = item.3
                return m
            }
            if scene.hasPrefix("transfer-secret") {
                // Passwords copied on a PC (sensitive): one here already, one still on the PC.
                var here = message(40, .text, sender: "fixture-macMini", text: "correct-horse-battery-staple")
                here.sensitive = true
                var remote = message(41, .text, sender: "fixture-windowsPc")
                remote.sensitive = true
                list += [here, remote]
            }
            messages = Self.fixtureStatuses(list.reversed())
            reachedEnd = true
            return
        }
        var list = [
            message(1, .text, sender: "fixture-windowsPc", text: "这版界面已经整理好了，发给你看看。"),
            message(2, .text, sender: "self", text: "收到，我在手机上确认一下。", mine: true),
            message(3, .image, sender: "fixture-macMini", name: "桌面预览.png", size: 1_800_000, state: .downloading),
            message(4, .file, sender: "fixture-windowsPc", name: "设计说明.pdf", size: 2_400_000, state: .placeholder),
            { var m = message(5, .folder, sender: "fixture-desktop", name: "项目素材", state: .stored); m.itemCount = 12; return m }(),
            message(6, .file, sender: "fixture-android", name: "会议纪要.docx", size: 86_000, state: .placeholder, proxy: "fixture-windowsPc"),
            message(7, .image, sender: "fixture-android", name: "白板照片.jpg", size: 2_100_000, state: .stored, proxy: "fixture-windowsPc"),
        ]
        // The folder page's states open the conversation's folder under the mockup's name.
        if scene == "transfer-folder-states", let index = list.firstIndex(where: { $0.kind == .folder }) {
            list[index].attachmentName = "现场资料"
        }
        if scene == "transfer-layout-edges" {
            // A file name longer than the bubble can show on one line (macOS screen recordings).
            list.append(message(8, .file, sender: "fixture-windowsPc", name: "Screen Recording 2026-11-09 at 12.37.04 PM.mov",
                                size: 1_500_000, state: .placeholder))
            // A text whose body could not be fetched (its source was unreachable).
            list.append(message(9, .text, sender: "fixture-windowsPc", state: .failed))
            // A portrait screenshot right below a text: the image must not take the text's taps.
            list.append(message(10, .text, sender: "self", text: "图片上面的文字", mine: true))
            var portrait = message(11, .image, sender: "self", name: "截图竖屏.png", size: 900_000, state: .stored, mine: true)
            portrait.contentID = "fixture-batch-23"  // the 9:16 art
            list.append(portrait)
        }
        messages = Self.fixtureStatuses((Self.imageStateFixture(scene) { n, kind, sender, name, text, size, state, proxy, mine in
            message(n, kind, sender: sender, name: name, text: text, size: size, state: state, proxy: proxy, mine: mine)
        } ?? list).reversed())
        reachedEnd = true
    }

    /// The core's `download_status` of the fixture records (it derives the same from the
    /// stored state, the source and phone-to-phone; file-bubble core item 4).
    private static func fixtureStatuses(_ list: [TransferMessage]) -> [TransferMessage] {
        list.map { message in
            var m = message
            guard !m.isMine else { m.downloadStatus = nil; return m }
            let availability = (m.availability ?? "").lowercased().replacingOccurrences(of: "_", with: "")
            m.downloadStatus = switch m.state {
            case .stored: .downloaded
            case .downloading: .downloading
            case .failed: m.isDirect ? .receiveFailed : availability == "sourceoffline" ? .sourceOffline : .downloadFailed
            case .cancelled: .cancelled
            default:
                ["evicted", "nosuchitem", "superseded", "unsupported"].contains(availability) ? .expired
                    : availability == "sourceoffline" ? .sourceOffline : .notDownloaded
            }
            return m
        }
    }

    /// Image messages in each preview state (thumbnail, thumbnail while the
    /// original downloads = 9003 with the scenes' 40 %, waiting, expired,
    /// offline, failed). "design-thumbnail" content ids draw the fixture picture.
    private static func imageStateFixture(
        _ scene: String,
        _ message: @MainActor (Int, TransferMessageKind, String, String?, String?, Int64?, AttachmentState?, String?, Bool) -> TransferMessage
    ) -> [TransferMessage]? {
        func image(_ n: Int, sender: String, _ preview: TransferImagePreview, thumbnail: Bool = false,
                   state: AttachmentState = .placeholder) -> TransferMessage {
            var m = message(n, .image, sender, "image.png", nil, 8_400_000, state, nil, false)
            m.contentID = thumbnail ? "design-thumbnail-\(n)" : "design-image-\(n)"
            m.imagePreview = preview
            m.imageAspect = thumbnail ? 1.6 : nil
            m.availability = preview == .expired ? "NoSuchItem" : preview == .offline ? "SourceOffline" : "Ready"
            return m
        }
        switch scene {
        case "transfer-thumbnails":
            return [
                message(1, .text, "fixture-windowsPc", nil, "截图发你了，原图比较大。", nil, nil, nil, false),
                image(2, sender: "fixture-windowsPc", .thumbnail, thumbnail: true),
                image(3, sender: "fixture-windowsPc", .thumbnail, thumbnail: true, state: .downloading),
                image(4, sender: "fixture-windowsPc", .loading),
                image(5, sender: "fixture-windowsPc", .expired),
            ]
        case "transfer-image-unavailable":
            return [
                image(1, sender: "fixture-desktop", .offline),
                image(2, sender: "fixture-windowsPc", .failed),
                image(4, sender: "fixture-windowsPc", .expired),
            ]
        default:
            return nil
        }
    }

    /// Phone-to-phone design states (native-rewrite.md「手机点对点传输」, mockup scenes
    /// transfer-p2p-*): the same conversation seen by the phone that sends straight to
    /// Android and by the phone that receives. Progress is set on the session fixture.
    private func p2pFixture(_ scene: String,
                            _ message: (Int, TransferMessageKind, String, String?, String?, Int64?, AttachmentState?, String?, Bool, String?) -> TransferMessage)
        -> [TransferMessage] {
        func item(_ n: Int, _ kind: TransferMessageKind, sender: String, name: String? = nil, text: String? = nil, size: Int64? = nil,
                  state: AttachmentState? = nil, proxy: String? = nil, mine: Bool = false, operation: String? = nil) -> TransferMessage {
            message(100 + n, kind, sender, name, text, size, state, proxy, mine, operation)
        }
        func direct(_ m: TransferMessage, _ peer: String = "fixture-android") -> TransferMessage {
            var m = m
            m.directPeerID = peer
            m.directPeerName = "Android"
            return m
        }
        func progress(_ n: Int, _ value: UInt64, sending: Bool) {
            core.recordProgress[Int64(9100 + n)] = sending ? TransferOperationProgress(sent: value, total: 100)
                : TransferOperationProgress(received: value, total: 100)
        }
        let zip = "现场照片（原图）.zip", zipSize: Int64 = 486_000_000
        let history = [
            item(1, .file, sender: "fixture-windowsPc", name: "设计说明.pdf", size: 2_400_000, state: .placeholder),
            item(2, .file, sender: "fixture-android", name: "会议纪要.docx", size: 86_000, state: .placeholder, proxy: "fixture-windowsPc"),
        ]
        let later = item(20, .text, sender: "fixture-macMini", text: "下午三点开会，别忘了。")
        switch scene {
        case "transfer-p2p-none":
            return []
        case "transfer-p2p-only-pc", "transfer-p2p-only-phone", "transfer-p2p-only-offline":
            let peer = scene == "transfer-p2p-only-pc" ? "fixture-windowsPc" : scene == "transfer-p2p-only-phone" ? "fixture-android" : "fixture-desktop"
            return [item(1, .text, sender: peer, text: "这版界面已经整理好了，发给你看看。"),
                    item(2, .file, sender: peer, name: "设计说明.pdf", size: 2_400_000, state: .placeholder),
                    item(3, .text, sender: "self", text: "收到，我在手机上确认一下。", mine: true),
                    item(4, .image, sender: "self", name: "白板照片.jpg", size: 1_600_000, state: .stored, mine: true),
                    item(5, .text, sender: peer, text: "今天现场的照片整理好了吗？")]
        case let s where s.hasPrefix("transfer-p2p-receiv") || s == "transfer-p2p-cancelled-receiver":
            // This phone receives straight from Android.
            var image = direct(item(11, .image, sender: "fixture-android", name: "IMG_2041.jpg", size: 3_800_000, state: .stored))
            var file = direct(item(12, .file, sender: "fixture-android", name: zip, size: zipSize, state: .stored))
            switch scene {
            case "transfer-p2p-receiving-menu":
                image.state = .downloading; file.state = .downloading
                progress(11, 30, sending: false); progress(12, 45, sending: false)
            case "transfer-p2p-receive-failed":
                image.state = .failed; file.state = .failed
                image.deliveryFailure = "对方已断开，接收失败"; file.deliveryFailure = "对方已断开，接收失败"
            case "transfer-p2p-receive-cancelled":
                file.state = .cancelled
            default: break
            }
            let incoming = item(10, .text, sender: "fixture-android", text: "原图直接发给你，电脑上就不占地方了。")
            // The sender cancelled: the message is gone here too.
            let files = scene == "transfer-p2p-cancelled-receiver" ? [image] : [image, file]
            return history + [incoming] + files + [later]
        default:
            // This phone sends straight to Android.
            var text = item(10, .text, sender: "self", text: "整理好了，原图直接发到 Android 上。", mine: true)
            let image = direct(item(11, .image, sender: "self", name: "IMG_2041.jpg", size: 3_800_000, state: .stored, mine: true))
            var file = direct(item(12, .file, sender: "self", name: zip, size: zipSize, state: .stored, mine: true, operation: "fixture-p2p-zip"))
            file.delivery = .sending
            progress(12, 45, sending: true)
            switch scene {
            case "transfer-p2p-sending-menu":
                text.text = "封面图发电脑，原图直接发到 Android 上。"
                var cover = item(11, .image, sender: "self", name: "封面.jpg", size: 1_200_000, state: .stored, mine: true, operation: "fixture-p2p-cover")
                cover.delivery = .sending
                progress(11, 30, sending: true)
                return history + [text, cover, file, later]
            case "transfer-p2p-cancelled-sender":
                return history + [text, image, later]
            case "transfer-p2p-send-failed":
                file.delivery = .failed
                file.deliveryFailure = "对方已断开，发送失败"
            case "transfer-p2p-send-cancelled-by-peer":
                file.delivery = .cancelled
            case "transfer-p2p-all-offline":
                file.delivery = .sent
            default: break
            }
            return history + [text, image, file, later]
        }
    }

    /// DEBUG design states: long-press「取消发送」removes the message (and the rest of
    /// its share);「取消接收」leaves it「已取消」, as the core does on both sides.
    func fixtureCancel(_ message: TransferMessage, sending: Bool) {
        if sending {
            messages.removeAll { $0.id == message.id || (message.operationID != nil && $0.operationID == message.operationID) }
        } else if let index = messages.firstIndex(where: { $0.id == message.id }) {
            messages[index].state = .cancelled
            messages[index].downloadStatus = .cancelled
        }
    }
    #endif

    var hasMore: Bool { !reachedEnd }

    func loadInitial() {
        guard messages.isEmpty else { return }
        loadMore()
    }

    func reload() {
        #if DEBUG
        // Design-state screenshots keep their fixture when the real session comes up.
        if DesignScene.hasPrefix("transfer") { return }
        #endif
        richCache.removeAll()
        #if OINK_MAIN_APP
        if messages.contains(where: \.sensitive) { ImageThumbnails.purge() }
        #endif
        messages = []
        oldestCreatedMs = nil
        oldestID = nil
        reachedEnd = false
        loadInitial()
    }

    func loadMore() {
        #if OINK_MOBILE_CORE_FFI
        // History needs the running session; `sessionRevision` reloads it
        // once the session is up.
        guard !isLoading, hasOlder, core.hasLiveSession else { return }
        isLoading = true
        olderLoadFailed = false
        Task { @MainActor in
            defer { isLoading = false }
            do {
                if core.sharedHistoryHasMore() { try await core.loadOlderSharedHistory() }
                let page = try core.transferPage(createdMs: oldestCreatedMs, id: oldestID, limit: 20)
                let known = Set(messages.map(\.id))
                messages.append(contentsOf: page.messages.filter { !known.contains($0.id) })
                oldestCreatedMs = page.nextCreatedMs ?? oldestCreatedMs
                oldestID = page.nextID ?? oldestID
                reachedEnd = page.nextID == nil && !core.sharedHistoryHasMore()
                sharedBoundary = core.sharedHistoryBoundary()
            } catch { olderLoadFailed = true }
        }
        #endif
    }

    /// Reloads only the records the core reported as changed.
    func apply(_ change: IOSTransferChange) {
        #if OINK_MOBILE_CORE_FFI
        sharedBoundary = core.sharedHistoryBoundary()
        guard !change.recordIDs.isEmpty else { reload(); return }
        for id in change.recordIDs {
            let key = String(id)
            let fetched: TransferMessage?
            do { fetched = try core.transferMessage(id) } catch { continue }
            guard let message = fetched else {
                richCache.removeValue(forKey: key)
                #if OINK_MAIN_APP
                if messages.contains(where: { $0.id == key && $0.sensitive }) { ImageThumbnails.purge() }
                #endif
                messages.removeAll { $0.id == key }
                continue
            }
            // Folder children belong to the folder viewer, not the list.
            guard message.parentID == nil else { continue }
            if let index = messages.firstIndex(where: { $0.id == key }) {
                messages[index] = message
            } else if reachedEnd || messages.last.map({ Self.newer(message, than: $0) }) ?? true {
                let index = messages.firstIndex(where: { Self.newer(message, than: $0) }) ?? messages.endIndex
                messages.insert(message, at: index)
            }
        }
        messages.sort(by: Self.newer)
        #endif
    }

    private static func newer(_ a: TransferMessage, than b: TransferMessage) -> Bool {
        if a.createdAt != b.createdAt { return a.createdAt > b.createdAt }
        if a.historyOrder != b.historyOrder { return (a.historyOrder ?? "") > (b.historyOrder ?? "") }
        return (a.recordID ?? 0) > (b.recordID ?? 0)
    }

    /// The rich text kept with text message `message` (user 2026-11-12): the
    /// bubble renders `displayHTML`; 复制 writes `html` / `rtf` with the text.
    /// Nil when it has none here (yet: a change of the record follows its
    /// arrival).
    func richText(_ message: TransferMessage) -> TransferRichText? {
        guard message.kind == .text, message.hasRichText else { return nil }
        #if DEBUG
        if let fixture = fixtureRich[message.id] { return fixture }
        #endif
        return core.transferRichText(message)
    }

    /// The rich text the bubble draws and the menu offers 复制文本 for; nil for plain text, for
    /// HTML that kept no formatting (RichContent), and while the rich text is not here yet (not
    /// remembered then: the record's change on arrival builds the row again).
    func richContent(_ message: TransferMessage) -> RichContent? {
        guard message.kind == .text, message.hasRichText else { return nil }
        if let cached = richCache[message.id] { return cached }
        guard let rich = richText(message), let content = RichContent(rich: rich) else { return nil }
        richCache[message.id] = content
        return content
    }

    func delete(_ message: TransferMessage) {
        #if OINK_MOBILE_CORE_FFI
        do {
            try core.deleteTransferMessage(message)
            richCache.removeValue(forKey: message.id)
            #if OINK_MAIN_APP
            if message.sensitive { ImageThumbnails.purge() }
            #endif
            messages.removeAll { $0.id == message.id }
        } catch { errorMessage = error.localizedDescription }
        #endif
    }

    /// An image bubble appeared: fetch its thumbnail when the core wants one.
    func requestThumbnail(_ message: TransferMessage) {
        #if OINK_MOBILE_CORE_FFI
        guard !message.isMine, message.kind == .image,
              message.imagePreview == .placeholder || message.imagePreview == .failed else { return }
        core.requestTransferThumbnail(message)
        #endif
    }

    /// `open: true` downloads if needed and opens the file with QuickLook
    /// when it is complete (CoreSession.fileToOpen); `open: false` only
    /// downloads.
    func download(_ message: TransferMessage, open: Bool) {
        #if OINK_MOBILE_CORE_FFI
        do { try core.startTransferDownload(message, open: open) }
        catch { errorMessage = error.localizedDescription }
        #endif
    }
}
