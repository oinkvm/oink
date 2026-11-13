import Foundation

/// The items Quick Look pages through when a picture, video or file is opened (user
/// 2026-11-11), in list order: every one this phone can show, plus pictures not downloaded
/// yet that have a thumbnail and are under 16 MB (shown with the thumbnail, downloaded when
/// reached). Folders, texts and items with nothing to show yet are left out; tapping those
/// downloads them first, as before. The conversation's sequence takes a batch as its single
/// items in their order; a folder page's takes the files of its level.
enum QuickLookSequence {
    /// A picture not downloaded yet pages in with its thumbnail when it is smaller than this.
    static let thumbnailOriginalLimit: Int64 = 16_000_000

    struct Entry: Equatable {
        let messageID: String
        let url: URL
        let title: String?
        /// The thumbnail stands in for a picture that is not downloaded yet.
        let standIn: Bool
    }

    /// The conversation's sequence; `ordered` oldest first. A folder's files are not in it.
    /// `original`: a message's downloaded file; `thumbnail`: a picture's thumbnail file;
    /// `canPreview`: Quick Look can show the file.
    static func entries(_ ordered: [TransferMessage], original: (TransferMessage) -> URL?, thumbnail: (TransferMessage) -> URL?,
                        canPreview: (URL) -> Bool) -> [Entry] {
        ordered.filter { $0.parentID == nil }.compactMap { entry($0, original: original, thumbnail: thumbnail, canPreview: canPreview) }
    }

    /// One message's entry, nil when it is not in a sequence.
    static func entry(_ message: TransferMessage, original: (TransferMessage) -> URL?, thumbnail: (TransferMessage) -> URL?,
                      canPreview: (URL) -> Bool) -> Entry? {
        switch message.kind {
        case .image, .file, .richText: break
        case .text, .folder: return nil
        }
        if let url = original(message), canPreview(url) {
            return Entry(messageID: message.id, url: url, title: message.attachmentName, standIn: false)
        }
        guard message.kind == .image, let size = message.byteSize, size < thumbnailOriginalLimit,
              let url = thumbnail(message), canPreview(url) else { return nil }
        return Entry(messageID: message.id, url: url, title: message.attachmentName, standIn: true)
    }

    /// Where the opened message is in the sequence.
    static func index(of messageID: String, in entries: [Entry]) -> Int? {
        entries.firstIndex { $0.messageID == messageID }
    }

    /// The downloaded file of a message, when it is there.
    static func storedFile(_ message: TransferMessage) -> URL? {
        guard message.state == .stored, let path = message.localPath, FileManager.default.fileExists(atPath: path) else { return nil }
        return URL(fileURLWithPath: path)
    }

    /// The PC's thumbnail of a picture, when it is there.
    static func thumbnailFile(_ message: TransferMessage) -> URL? {
        guard message.imagePreview == .thumbnail, let path = message.thumbnailPath, FileManager.default.fileExists(atPath: path) else {
            return nil
        }
        return URL(fileURLWithPath: path)
    }
}
