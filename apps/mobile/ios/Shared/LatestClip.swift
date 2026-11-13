import AppIntents
import Foundation
import UniformTypeIdentifiers

/// What the one-tap copy shortcut hands to Shortcuts: the group's latest
/// shared text or image. Files are not copied; they stay on the transfer page.
enum LatestClip: Codable, Equatable {
    case text(String)
    /// A downloaded image in the transfer store (App Group container). The
    /// conversation shows the same file, so it is read, never moved.
    case image(path: String, mime: String?)

    #if OINK_MOBILE_CORE_FFI
    /// The core's answer as a shortcut result. Nothing current (an empty
    /// clipboard or an offline source), files and oversized images are
    /// failures the shortcut reports.
    static func from(_ current: MobileCurrentClipboard?) throws -> LatestClip {
        switch current {
        case nil:
            throw LatestClipFailure.empty
        // The shortcut hands Copy to Clipboard one plain-text file, which
        // writes the pasteboard itself: the rich text cannot go with it.
        case .text(let text, _, _, _, _):
            guard !text.isEmpty else { throw LatestClipFailure.empty }
            return .text(text)
        case .image(let path, let mime, _, _, _):
            return .image(path: path, mime: mime)
        case .imageTooLarge(let size, let limit):
            throw LatestClipFailure.imageTooLarge(size: size, limit: limit)
        case .unsupported(let kind):
            throw kind == .file || kind == .folder ? LatestClipFailure.file : LatestClipFailure.unsupported
        }
    }
    #endif

    /// The Shortcuts output. Text is a UTF-8 plain-text file so one output
    /// type carries both kinds; Copy to Clipboard puts its text (or the
    /// image data) on the pasteboard. The data is read here, before the
    /// background session is released.
    func intentFile() throws -> IntentFile {
        switch self {
        case .text(let text):
            return IntentFile(data: Data(text.utf8), filename: "Oink.txt", type: .utf8PlainText)
        case .image(let path, let mime):
            let url = URL(fileURLWithPath: path)
            let type = mime.flatMap { UTType(mimeType: $0) } ?? UTType(filenameExtension: url.pathExtension) ?? .png
            let data: Data
            do { data = try Data(contentsOf: url) } catch { throw LatestClipFailure.interrupted }
            return IntentFile(data: data, filename: url.lastPathComponent, type: type)
        }
    }
}

/// The session side of the one-tap copy shortcut (CoreSession in the app).
@MainActor
protocol LatestClipSession: AnyObject {
    /// Holds the session (the app's live one, or the one started for the
    /// intent) until `finishBackgroundOperation`.
    func beginBackgroundOperation()
    func latestSharedClip() async throws -> LatestClip
    /// Releases the session unless the scene, audio or another intent holds it.
    func finishBackgroundOperation() async
}

extension LatestClip {
    /// The shortcut's run. The hold starts before the session is looked up
    /// or started and ends after the result's bytes are read, on failures too.
    @MainActor
    static func shortcutFile(from session: some LatestClipSession) async throws -> IntentFile {
        session.beginBackgroundOperation()
        do {
            let file = try await session.latestSharedClip().intentFile()
            await session.finishBackgroundOperation()
            return file
        } catch {
            await session.finishBackgroundOperation()
            throw error
        }
    }
}

/// Why the one-tap copy returned nothing; the message is what Shortcuts shows.
enum LatestClipFailure: Error, Equatable, LocalizedError, CustomLocalizedStringResourceConvertible {
    /// No paired computer with clipboard sharing is online.
    case noComputer
    /// Nothing is shared right now, or its source went offline.
    case empty
    case file
    /// Rich text without a plain-text form.
    case unsupported
    case imageTooLarge(size: UInt64, limit: UInt64)
    /// The answer or the download was cut off.
    case interrupted

    var message: String {
        switch self {
        case .noComputer:
            AppText.pick("没有在线的电脑。请确认电脑上的 Oink 已打开，并且共享剪贴板。",
                         "No computer is online. Make sure Oink is running on your computer and shares its clipboard.")
        case .empty:
            AppText.pick("现在没有可复制的共享内容。", "Nothing is shared right now.")
        case .file:
            AppText.pick("最新共享的是文件。文件请在传输页下载。",
                         "The latest shared item is a file. Download files from the Transfer page.")
        case .unsupported:
            AppText.pick("最新共享的内容不是文字或图片。", "The latest shared item isn't text or an image.")
        case .imageTooLarge(let size, let limit):
            AppText.pick("图片太大（\(formatBytes(Int64(size)))，上限 \(formatBytes(Int64(limit)))），请在传输页下载。",
                         "The image is too large (\(formatBytes(Int64(size))), limit \(formatBytes(Int64(limit)))). Download it from the Transfer page.")
        case .interrupted:
            AppText.pick("连接中断，没有拿到内容，请重试。", "The connection was interrupted. Try again.")
        }
    }
    var errorDescription: String? { message }
    var localizedStringResource: LocalizedStringResource { LocalizedStringResource(stringLiteral: message) }
}
