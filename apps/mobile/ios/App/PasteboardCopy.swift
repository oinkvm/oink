import UIKit
import UniformTypeIdentifiers

/// Long-press 复制 on the transfer page. Oink writes the pasteboard itself so
/// the copy carries the Oink clip marker next to the content: Universal
/// Clipboard brings both to a Mac on the same Apple ID, and the Mac's Oink
/// recognises the clip it announced instead of sending it back (sending-side
/// deduplication, `oink_foundation::clipboard_marker`). Never `localOnly`:
/// the copy is meant to reach Universal Clipboard like any other copy.
enum PasteboardCopy {
    /// `oink_foundation::clipboard_marker::MARKER_APPLE`.
    static let markerType = "com.hachiwii.oink.clip"
    /// nspasteboard.org convention for passwords and other secrets: clipboard
    /// managers and history tools leave such items out.
    static let concealedType = "org.nspasteboard.ConcealedType"

    /// Whether the message has content to copy now: text, or an image whose
    /// data is downloaded. Files and folders are never copied (their content
    /// is opened or shared, not pasted).
    static func canCopy(_ message: TransferMessage) -> Bool {
        switch message.kind {
        case .text: return message.text != nil
        case .image: return message.state == .stored && message.localPath != nil
        case .file, .folder, .richText: return false
        }
    }

    /// The pasteboard item of a message: the content under its type plus the
    /// record's clip marker; a record without a clip identity (a direct
    /// share) has no marker. Nil when there is nothing to copy. `rich`: a
    /// text message's rich text, written in the same item as `public.html`
    /// (and `public.rtf` when the source was RTF) next to the plain text
    /// (user 2026-11-12). The long-press menu's 复制 passes it
    /// (`copyRich`), 复制文本 does not (`copyPlain`).
    static func item(for message: TransferMessage, rich: TransferRichText? = nil) throws -> [String: Any]? {
        guard canCopy(message) else { return nil }
        var content: [String: Any]
        switch message.kind {
        case .text:
            content = [UTType.utf8PlainText.identifier: message.text ?? ""]
            if let rich {
                content[UTType.html.identifier] = rich.html
                if let rtf = rich.rtf, let data = rtf.data(using: .utf8) { content[UTType.rtf.identifier] = data }
            }
        case .image:
            guard let path = message.localPath, let type = imageType(path: path, mime: message.uti) else { return nil }
            content = [type.identifier: try Data(contentsOf: URL(fileURLWithPath: path))]
        case .file, .folder, .richText:
            return nil
        }
        var result = item(content, marker: message.clipMarker)
        if message.sensitive { result[concealedType] = Data() }
        return result
    }

    static func item(_ content: [String: Any], marker: Data?) -> [String: Any] {
        var item = content
        if let marker { item[markerType] = marker }
        return item
    }

    /// Writes one item, replacing the pasteboard's contents.
    static func write(_ item: [String: Any], to pasteboard: UIPasteboard = .general) {
        pasteboard.setItems([item])
    }

    /// 复制: a text message with its rich text and plain text; other messages
    /// as they are. False when there is nothing to copy.
    @discardableResult
    static func copyRich(_ message: TransferMessage, rich: TransferRichText?, to pasteboard: UIPasteboard = .general) throws -> Bool {
        guard let item = try item(for: message, rich: rich) else { return false }
        write(item, to: pasteboard)
        return true
    }

    /// 复制文本: a text message's plain text only.
    @discardableResult
    static func copyPlain(_ message: TransferMessage, to pasteboard: UIPasteboard = .general) throws -> Bool {
        try copyRich(message, rich: nil, to: pasteboard)
    }

    /// The downloaded data's type: from the file's extension, else the
    /// record's mime type; only image types qualify.
    static func imageType(path: String, mime: String?) -> UTType? {
        let candidates = [UTType(filenameExtension: (path as NSString).pathExtension), mime.flatMap { UTType(mimeType: $0) }]
        return candidates.compactMap { $0 }.first { $0.conforms(to: .image) }
    }
}
