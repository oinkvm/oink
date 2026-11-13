import Foundation
import ImageIO
import UniformTypeIdentifiers

// Host-side decisions kept free of UIKit and of the CoreSession singleton so
// the host unit tests run them directly.

#if OINK_MOBILE_CORE_FFI
/// The result of a submitted pairing code. Submitting only queues the PAKE
/// step: the core shows `.waiting` until the peer's verdict arrives in a later
/// snapshot (review L15). The attempt counts down when the core takes the
/// code, so a snapshot older than the submission (still showing the previous
/// attempt's `.wrong`) is not this attempt's verdict.
enum PairingCodeCheck: Equatable {
    case pending, wrong, settled

    static func outcome(submitted: PairView, current: PairView?) -> PairingCodeCheck {
        guard let current, current.terminal == nil else { return .settled }
        if current.feedback == .correct { return .settled }
        guard current.attemptsLeft < submitted.attemptsLeft else { return .pending }
        return current.feedback == .wrong ? .wrong : .pending
    }
}

/// Core share items for a host payload. Directories become folders (review
/// M30); images go as images with the size the user sees (ImageHeader), so the
/// own message shows at the original's proportions on every device. This is the
/// one place every send builds its items: the transfer page (photos, files,
/// camera), the share extension and open-with all go through `CoreSession.share`.
enum ShareItems {
    static func items(for payload: SharePayload) -> [MobileShareItem] {
        var items: [MobileShareItem] = []
        // The text with every rich format it came with (user 2026-11-12).
        if let text = payload.text { items.append(.text(text: text, html: payload.html, rtf: payload.rtf)) }
        for file in payload.files {
            let values = try? file.resourceValues(forKeys: [.isDirectoryKey, .fileSizeKey])
            if values?.isDirectory == true {
                items.append(.folder(path: file.path, name: file.lastPathComponent))
                continue
            }
            let size = values?.fileSize.map(UInt64.init) ?? 0
            let mime = UTType(filenameExtension: file.pathExtension)?.preferredMIMEType
            if let mime, mime.hasPrefix("image/") {
                // Both or neither (the core refuses one alone); none when ImageIO cannot read it.
                let shown = ImageHeader.displaySize(path: file.path)
                items.append(.image(path: file.path, encodedSize: size, mime: mime,
                                    staged: payload.staged, width: shown?.width, height: shown?.height))
            } else {
                items.append(.file(path: file.path, name: file.lastPathComponent, size: size, mime: mime,
                                   staged: payload.staged))
            }
        }
        return items
    }
}
#endif

/// A picture's size as the user sees it, from the file header only (no decode):
/// pixel width and height, turned a quarter for EXIF / HEIF orientations 5–8.
enum ImageHeader {
    static func displaySize(path: String) -> (width: UInt32, height: UInt32)? {
        guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL,
                                                      [kCGImageSourceShouldCache: false] as CFDictionary),
              let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let width = (properties[kCGImagePropertyPixelWidth] as? NSNumber)?.uint32Value,
              let height = (properties[kCGImagePropertyPixelHeight] as? NSNumber)?.uint32Value,
              width > 0, height > 0 else { return nil }
        let turned = ((properties[kCGImagePropertyOrientation] as? NSNumber)?.intValue ?? 1) >= 5
        return turned ? (height, width) : (width, height)
    }
}

/// The transfer page's text field after a text send (review L17): it is
/// cleared only once the send succeeded, and only when the user has not typed
/// something else meanwhile; a failed send keeps the text for another try.
enum ComposerDraft {
    static func after(sending sent: String, current: String, succeeded: Bool) -> String {
        succeeded && current == sent ? "" : current
    }
}

/// One share extension staging directory, owned by its process through an
/// advisory lock (review L17). The lock dies with the process, so the app
/// removes exactly the directories of extensions that ended without cleaning
/// up, never one still being filled or sent.
final class ShareStagingLease {
    let directory: URL
    private var descriptor: Int32

    private static let lockName = ".owner"

    init(in parent: URL) throws {
        let name = UUID().uuidString
        // Built under a hidden name and renamed once locked, so the cleanup
        // never sees a live directory before its lock exists.
        let building = parent.appendingPathComponent("." + name, isDirectory: true)
        let directory = parent.appendingPathComponent(name, isDirectory: true)
        try FileManager.default.createDirectory(at: building, withIntermediateDirectories: true,
            attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication])
        let descriptor = open(building.appendingPathComponent(Self.lockName).path, O_CREAT | O_RDWR, 0o600)
        guard descriptor >= 0, flock(descriptor, LOCK_EX | LOCK_NB) == 0 else {
            if descriptor >= 0 { close(descriptor) }
            try? FileManager.default.removeItem(at: building)
            throw OinkFailure(message: "无法创建分享暂存区。")
        }
        do {
            try FileManager.default.moveItem(at: building, to: directory)
        } catch {
            close(descriptor)
            try? FileManager.default.removeItem(at: building)
            throw error
        }
        self.directory = directory
        self.descriptor = descriptor
    }

    /// Deletes the directory and releases the lock.
    func remove() {
        try? FileManager.default.removeItem(at: directory)
        release()
    }

    private func release() {
        guard descriptor >= 0 else { return }
        close(descriptor)
        descriptor = -1
    }

    deinit { release() }

    /// Removes the staging directories whose owner process is gone.
    static func removeAbandoned(in parent: URL) {
        guard let entries = try? FileManager.default.contentsOfDirectory(at: parent, includingPropertiesForKeys: nil) else { return }
        for entry in entries where !entry.lastPathComponent.hasPrefix(".") {
            let descriptor = open(entry.appendingPathComponent(lockName).path, O_RDWR)
            if descriptor < 0 {
                // A directory without a lock file was left by a build that
                // did not lock its staging; no live process can own it.
                if errno == ENOENT { try? FileManager.default.removeItem(at: entry) }
                continue
            }
            defer { close(descriptor) }
            guard flock(descriptor, LOCK_EX | LOCK_NB) == 0 else { continue }
            try? FileManager.default.removeItem(at: entry)
        }
    }
}
