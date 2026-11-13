import Combine
import Foundation

/// A downloaded file to show with QuickLook (iOS cannot `openURL` a file in
/// the app container).
struct IOSFileToOpen: Identifiable, Equatable {
    let id: Int64
    let url: URL
    let name: String
    let mime: String?
}

#if OINK_MOBILE_CORE_FFI
extension TransferModel {
    convenience init() { self.init(core: CoreSession.shared) }
}

extension CoreSession: TransferHistorySource {
    var transferChanges: AnyPublisher<IOSTransferChange, Never> { $transferChange.eraseToAnyPublisher() }
    var sessionRevisions: AnyPublisher<Int, Never> { $sessionRevision.eraseToAnyPublisher() }
    var hasLiveSession: Bool { mobileSession != nil }
    func sharedHistoryBoundary() -> Date? {
        guard let ms = try? mobileSession?.transferHistoryBoundaryMs() else { return nil }
        return Date(timeIntervalSince1970:Double(ms)/1000)
    }
    func sharedHistoryHasMore() -> Bool { (try? mobileSession?.transferHistoryHasMore()) ?? false }
    func loadOlderSharedHistory() async throws { try await requireLiveSession().transferLoadOlderHistory() }
}

@MainActor
extension CoreSession {
    // MARK: History (the running session's own store, never a second one)

    func transferPage(createdMs: Int64?, id: Int64?, limit: UInt32 = UInt32(TransferPageSize.messages)) throws -> CoreTransferPage {
        let page = try requireLiveSession().transferPage(cursorCreatedMs: createdMs, cursorId: id, limit: limit)
        return CoreTransferPage(messages: page.messages.map(transferMessage(from:)),
                                nextCreatedMs: page.nextCreatedMs, nextID: page.nextId)
    }

    /// One record by id; nil once deleted.
    func transferMessage(_ id: Int64) throws -> TransferMessage? {
        try requireLiveSession().transferMessage(messageId: id).map(transferMessage(from:))
    }

    /// The rich text kept with a text message (2026-11-12); nil when it has none here.
    func transferRichText(_ message: TransferMessage) -> TransferRichText? {
        guard let id = message.recordID,
              let rich = try? requireLiveSession().transferRichText(messageId: id) else { return nil }
        return TransferRichText(displayHTML: rich.displayHtml, html: rich.html, rtf: rich.rtf)
    }

    /// Long-press delete: the message, its folder children and their files
    /// (D16); a running download of it stops.
    func deleteTransferMessage(_ message: TransferMessage) throws {
        guard let id = message.recordID else { throw OinkFailure(message: "消息编号无效。") }
        try requireLiveSession().transferDeleteMessage(messageId: id)
        recordProgress[id] = nil
        // Deleting a folder stops its children's downloads too.
        activeDownloads = activeDownloads.filter { (try? transferMessage($0))?.state == .downloading }
        publishTransferChange([id])
    }

    /// Settings → 清理缓存. Message metadata stays; attachments become
    /// placeholders (own ones are evicted).
    func clearTransferCache() throws {
        try requireLiveSession().transferClearCachedFiles()
        recordProgress.removeAll()
        activeDownloads.removeAll()
        publishTransferChange([])
    }

    /// Bytes 清理缓存 would free.
    func transferCacheBytes() throws -> Int64 {
        Int64(clamping: try requireLiveSession().transferCachedBytes())
    }

    /// Settings → 清理传输记录: deletes every finished record and its files.
    /// Transfers still moving keep their records, progress and downloads; the
    /// page reloads to them (the core also reports the deleted records as
    /// `transferRemoved`).
    func clearTransferHistory() throws {
        try requireLiveSession().transferClearHistory()
        publishTransferChange([])
    }

    /// Whether the conversation has a record (清理传输记录 is grey without one).
    func hasTransferHistory() -> Bool {
        guard let owner = mobileSession,
              let page = try? owner.transferPage(cursorCreatedMs: nil, cursorId: nil, limit: 1) else { return false }
        return !page.messages.isEmpty
    }

    // MARK: Downloads and auto-open

    /// `open: true` (tap a file) downloads if needed and then opens it with
    /// QuickLook through `fileToOpen`; it replaces any earlier pending open.
    /// `open: false` (download button) only downloads and clears the slot.
    /// Already downloaded files also go through here with `open: true`.
    func startTransferDownload(_ message: TransferMessage, open: Bool) throws {
        let owner = try requireLiveSession()
        guard let messageID = message.recordID else { throw OinkFailure(message: "消息编号无效。") }
        do {
            try owner.transferDownload(messageId: messageID, open: open)
        } catch MobileError.Transfer(let reason, _) {
            throw OinkFailure(message: TransferFailureText.text(reason))
        }
    }

    /// An image bubble became visible: ask its PC for a thumbnail. The core
    /// decides whether one is needed (large or unknown size, not downloaded,
    /// source online) and ignores the call otherwise. Its result arrives as a
    /// pushed transfer event.
    func requestTransferThumbnail(_ message: TransferMessage) {
        guard let owner = mobileSession, let id = message.recordID else { return }
        _ = try? owner.transferRequestThumbnail(messageId: id)
    }

    /// Call when leaving the transfer page (or the folder viewer), so a late
    /// completion does not open a file the user no longer waits for.
    func cancelTransferAutoOpen() {
        try? mobileSession?.cancelTransferAutoOpen()
    }

    private func takeAutoOpen() {
        guard let owner = mobileSession, let ready = try? owner.takeTransferAutoOpen() else { return }
        fileToOpen = IOSFileToOpen(id: ready.recordId, url: URL(fileURLWithPath: ready.path),
                                   name: ready.displayName, mime: ready.mime)
    }

    // MARK: Folders

    /// First page of a directory (`more: false`) or the next page (also 重试
    /// on a failed page). It ends in a page or in a failure the folder view
    /// shows (`folderListings.failures`), never in a thrown error.
    func requestTransferDirectory(_ folder: TransferMessage, node: UInt64 = 0, more: Bool = false) {
        guard let folderID = folder.recordID else { return }
        let key = IOSDirectoryKey(folderMessageID: folderID, directory: node)
        var version: UInt64?
        var offset: UInt32?
        if more {
            guard let page = transferDirectories[key], page.hasMore else { return }
            version = page.nextVersion
            offset = page.nextOffset
        }
        let operation = Data(randomID())
        folderListings.requested(Self.hex(operation), key: key, append: more)
        do {
            let owner = try requireLiveSession()
            guard let contentID = folder.contentID else { throw OinkFailure(message: "文件夹标识不完整。") }
            try owner.transferListDirectory(operationId: operation, contentId: contentID, node: node,
                                            cursorVersion: version, cursorOffset: offset)
        } catch {
            folderListings.notRequested(Self.hex(operation), failure: TransferDirectoryFailure(
                text: AppText.pick("无法获取文件夹内容：", "Cannot read the folder: ") + error.oinkMessage, retryable: true))
        }
    }

    /// Downloads one file of a folder listing as a child record of the folder
    /// (never a new conversation message). Returns the child record id.
    @discardableResult
    func downloadDirectoryFile(_ folder: TransferMessage, entry: TransferDirectoryEntry, open: Bool) throws -> Int64 {
        let owner = try requireLiveSession()
        guard let folderID = folder.recordID else { throw OinkFailure(message: "文件夹标识不完整。") }
        let child = try owner.transferDownloadDirectoryFile(operationId: Data(randomID()), folderMessageId: folderID,
            entry: Self.directoryEntry(entry), open: open)
        return child
    }

    /// Downloaded children of a folder by listing entry id (download state,
    /// local path); progress is `recordProgress[child.recordID]`.
    func folderChildren(_ folder: TransferMessage) throws -> [UInt64: TransferMessage] {
        guard let folderID = folder.recordID else { return [:] }
        var result: [UInt64: TransferMessage] = [:]
        for child in try requireLiveSession().transferFolderChildren(folderMessageId: folderID) {
            if let node = child.directoryNode { result[node] = transferMessage(from: child) }
        }
        return result
    }

    /// A picture row of a computer's folder became visible: ask that computer for its
    /// preview, the same way as a picture message's (file-bubble SPEC core item 2). The
    /// answer comes back on the listing entry (`TransferDirectoryEntry.thumbnailPath`).
    /// Phone-to-phone folders never ask: their files are here and the page makes the
    /// thumbnails itself.
    /// The core answers None for entries it does not fetch (not a picture name, a
    /// phone-to-phone folder) and ignores entries already asked for or finished.
    func requestDirectoryThumbnail(_ folder: TransferMessage, entry: TransferDirectoryEntry) {
        guard let owner = mobileSession, let folderID = folder.recordID, !folder.isDirect else { return }
        _ = try? owner.transferRequestDirectoryThumbnail(folderMessageId: folderID, entry: Self.directoryEntry(entry))
    }

    /// A listing entry handed back to the core (its `child` is ignored there).
    private static func directoryEntry(_ entry: TransferDirectoryEntry) -> MobileDirectoryEntry {
        MobileDirectoryEntry(id: entry.id, parent: entry.parent, name: entry.name, kind: entry.kind, size: entry.size,
                             version: entry.version, child: nil)
    }

    // MARK: Events

    func applyTransferEvent(_ event: MobileEvent) {
        switch event {
        case .transferMessage:
            transferRevision &+= 1
        case .transferAnnouncement(let recordID, _, let parentID, let autoOpenReady, _):
            // A download ends with an announcement of its record (M28).
            if activeDownloads.contains(recordID), (try? transferMessage(recordID))?.state != .downloading {
                activeDownloads.remove(recordID)
            }
            publishTransferChange(parentID.map { [recordID, $0] } ?? [recordID])
            if autoOpenReady { takeAutoOpen() }
        case .transferDownloadProgress(_, let received, let total, let recordID):
            activeDownloads.insert(recordID)
            var value = recordProgress[recordID] ?? TransferOperationProgress(total: total)
            value.received = received
            if let total { value.total = total }
            recordProgress[recordID] = value
            transferRevision &+= 1
        case .transferRateSample(let operationID, let bytesPerSecond):
            let key = Self.hex(operationID)
            var value = transferOperations[key] ?? TransferOperationProgress(total: nil)
            value.bytesPerSecond = bytesPerSecond
            transferOperations[key] = value
            transferRevision &+= 1
        case .transferSendProgress(let operationID, let sent, let total, let recordIDs):
            let key = Self.hex(operationID)
            var value = transferOperations[key] ?? TransferOperationProgress(total: total)
            value.sent = sent
            if let total { value.total = total }
            transferOperations[key] = value
            for id in recordIDs { recordProgress[id] = value }
            transferRevision &+= 1
        case .deliveryEvent(let operationID, let status, let recordIDs, let state, let failure, _):
            let key = Self.hex(operationID)
            var value = transferOperations[key] ?? TransferOperationProgress(total: nil)
            value.status = status
            transferOperations[key] = value
            for id in recordIDs { recordProgress[id] = nil }
            publishTransferChange(recordIDs)
            settleDelivery(key, IOSDeliveryOutcome(state: state, failure: failure, recordIDs: recordIDs))
        case .transferError(let operationID, _, let recordID):
            noteTransferFailure()
            if let recordID {
                activeDownloads.remove(recordID)
                recordProgress[recordID] = nil
                publishTransferChange([recordID])
            }
            // `detail` is diagnostic text; the record's state carries the
            // outcome, including a source that no longer has the content.
            if let recordID {
                let availability = (try? transferMessage(recordID))?.availability
                transferError = availability.flatMap(TransferFailureText.availability) ?? "下载失败，请重试。"
            } else {
                transferError = "传输失败，请重试。"
            }
            transferRevision &+= 1
        // The core deleted records: an own share cancelled, or a phone-to-phone send
        // its sender cancelled. Reloading them finds them gone and drops them.
        case .transferRemoved(let recordIDs):
            for id in recordIDs { recordProgress[id] = nil; activeDownloads.remove(id) }
            publishTransferChange(recordIDs)
        // A phone-to-phone receive started or ended: its records appear or settle.
        case .transferDirectIncoming(_, _, _, let recordIDs, _, _, _):
            publishTransferChange(recordIDs)
        case .transferDirectEnded(_, let recordIDs, _, _):
            for id in recordIDs { recordProgress[id] = nil; activeDownloads.remove(id) }
            publishTransferChange(recordIDs)
        case .transferDirectoryPage(let directory, let entries, let nextOffset, let operationID, let folderID, let nextVersion):
            folderListings.page(Self.hex(operationID), folderID: folderID, directory: directory, entries: entries.map {
                TransferDirectoryEntry(id: $0.id, parent: $0.parent, name: $0.name, kind: $0.kind, size: $0.size, version: $0.version,
                                       child: $0.child.map(transferMessage(from:)))
            }, nextOffset: nextOffset, nextVersion: nextVersion)
            transferRevision &+= 1
        // The folder view stops loading and shows why (with 重试 when it can help).
        case .transferDirectoryFailed(let operationID, let folderID, let directory, let reason, let retryable, _):
            folderListings.failed(Self.hex(operationID), folderID: folderID, directory: directory,
                                  failure: TransferDirectoryFailure(text: TransferFailureText.folder(reason), retryable: retryable))
            transferRevision &+= 1
        default:
            break
        }
    }

    private func publishTransferChange(_ ids: [Int64]) {
        transferChange = IOSTransferChange(sequence: transferChange.sequence &+ 1, recordIDs: ids)
    }

    // MARK: Projection

    private func deviceName(_ id: String) -> String {
        if let card = cards?.cards.first(where: { $0.device == id }) { return card.isSelf ? "本机" : card.name }
        if id == mobileSession?.deviceId() { return "本机" }
        return (try? deviceFingerprint(device: id)) ?? id
    }

    func transferMessage(from value: MobileMessage) -> TransferMessage {
        let kind: TransferMessageKind
        switch value.kind.lowercased() {
        case "text": kind = .text
        case "image": kind = .image
        case "folder": kind = .folder
        case "richtext", "rich_text": kind = .richText
        default: kind = .file
        }
        let direct = value.directPeer != nil
        let state: AttachmentState?
        switch value.downloadState.lowercased() {
        case "complete": state = .stored
        case "downloading": state = .downloading
        // A phone-to-phone receive that ended early is never retried; cancelled by
        // either end it reads「已取消」.
        case "failed" where direct && !value.isOutgoing:
            state = value.deliveryFailure == .cancelled ? .cancelled : .failed
        case "failed":
            // A replaced or deleted source cannot be retried.
            state = TransferFailureText.availability(value.sourceAvailability) == nil
                || value.sourceAvailability.lowercased() == "sourceoffline" ? .failed : .unavailable
        default: state = value.sourceAvailability.lowercased() == "ready" ? .placeholder : .unavailable
        }
        let delivery: TransferDeliveryState? = value.deliveryState.map {
            switch $0 {
            case .sending: .sending
            case .sent: .sent
            // A share this phone cancels is removed; a cancelled one that stays was
            // cancelled by the receiving phone.
            case .failed: value.deliveryFailure == .cancelled ? .cancelled : .failed
            }
        }
        let failure: String? = if direct, delivery == .failed || state == .failed {
            TransferFailureText.direct(value.deliveryFailure, sending: value.isOutgoing)
                ?? Self.deliveryFailureText(value.deliveryFailure)
        } else if delivery == .failed {
            Self.deliveryFailureText(value.deliveryFailure)
        } else {
            nil
        }
        let preview: TransferImagePreview? = value.imagePreview.map {
            switch $0 {
            case .original: .original
            case .thumbnail: .thumbnail
            case .loading: .loading
            case .expired: .expired
            case .offline: .offline
            case .failed: .failed
            case .placeholder: .placeholder
            }
        }
        // An own picture's original as sent, a received one's thumbnail (file-bubble core 1).
        let aspect: Double? = {
            guard let width = value.imageWidth, let height = value.imageHeight, width > 0, height > 0 else { return nil }
            return Double(width) / Double(height)
        }()
        let status: TransferDownloadStatus? = value.downloadStatus.map {
            switch $0 {
            case .notDownloaded: .notDownloaded
            case .downloading: .downloading
            case .downloaded: .downloaded
            case .downloadFailed: .downloadFailed
            case .receiveFailed: .receiveFailed
            case .cancelled: .cancelled
            case .sourceOffline: .sourceOffline
            case .expired: .expired
            }
        }
        return TransferMessage(id: String(value.id),
            createdAt: Date(timeIntervalSince1970: Double(value.createdMs) / 1000),
            senderDeviceID: value.proxyId ?? value.senderId, senderName: deviceName(value.senderId),
            offerID: nil, sourceBoot: nil, operationID: Self.hex(value.operationId),
            itemID: value.itemId, representationID: value.representationId,
            originSenderID: value.senderId, proxyDeviceID: value.proxyId, proxyName: value.proxyId.map(deviceName),
            kind: kind, text: value.text, contentID: value.contentId, localPath: value.localPath,
            attachmentName: value.displayName, byteSize: Int64(clamping: value.size), uti: value.mime,
            state: state, availability: value.sourceAvailability, isMine: value.isOutgoing, historyOrder: value.historyOrder,
            itemCount: value.itemCount.map(Int.init), delivery: delivery,
            deliveryFailure: failure,
            parentID: value.parentId, directoryNode: value.directoryNode, sourceOnline: value.sourceOnline,
            clipMarker: value.clipMarker, sensitive: value.sensitive, imagePreview: preview,
            thumbnailPath: value.thumbnailPath, imageAspect: aspect, downloadStatus: status,
            directPeerID: value.directPeer, directPeerName: value.directPeerName,
            hasRichText: kind == .text && value.richTextFormat != nil)
    }
}
#endif
