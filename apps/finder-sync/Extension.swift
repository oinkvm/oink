import AppKit
import FinderSync
import UniformTypeIdentifiers

@objc(OinkFinderSync)
final class OinkFinderSync: FIFinderSync {
    private var menuDestination: URL?
    override init() {
        super.init()
        FIFinderSyncController.default().directoryURLs = [URL(fileURLWithPath: "/")]
    }
    private var preferences: [String: Any] {
        guard let root = pasteGroupContainer(),
              let bytes = try? Data(contentsOf: root.appendingPathComponent("paste-as-file.json")),
              let value = try? JSONSerialization.jsonObject(with: bytes) as? [String: Any] else { return [:] }
        return value
    }

    // Kept as a small factory so the menu contract can be tested without
    // starting Finder. The image is supplied by the extension bundle and is
    // selected for the current menu appearance.
    static func makePasteAsFileMenuItem(title: String, image: NSImage?) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: #selector(pasteAsFile(_:)), keyEquivalent: "")
        item.image = image
        return item
    }
    private func destination(_ kind: FIMenuKind) -> URL? {
        let controller = FIFinderSyncController.default()
        guard kind == .contextualMenuForContainer || kind == .contextualMenuForItems else { return nil }
        return pasteDestination(container: kind == .contextualMenuForContainer,
                                targeted: controller.targetedURL(), selected: controller.selectedItemURLs())
    }
    override func menu(for menuKind: FIMenuKind) -> NSMenu? {
        menuDestination = nil
        let prefs = preferences
        guard prefs["enabled"] as? Bool == true, let target = destination(menuKind) else { return nil }
        menuDestination = target
        let language = prefs["language"] as? String ?? "en"
        let title = ["zh-Hans": "粘贴为文件", "ja": "ファイルとして貼り付け", "ko": "파일로 붙여넣기"][language] ?? "Paste as File"
        let menu = NSMenu()
        menu.autoenablesItems = false
        let item = Self.makePasteAsFileMenuItem(title: title, image: pasteMenuImage())
        item.target = self
        // Inspect types only. Reading data while constructing a menu would render
        // delayed remote clipboard formats before the user requests a paste.
        item.isEnabled = NSPasteboard.general.types?.contains { type in
            ["public.file-url", "public.utf8-plain-text", "public.utf16-plain-text", "public.utf16-external-plain-text",
             "NSStringPboardType", "public.rtf", "NSRTFPboardType", "public.html", "NSHTMLPboardType", "NSTIFFPboardType",
             "NSFilesPromisePboardType", "com.apple.pasteboard.promised-file-url"].contains(type.rawValue)
            || UTType(type.rawValue)?.conforms(to: .image) == true
        } == true
        menu.addItem(item)
        return menu
    }

    private func pasteMenuImage() -> NSImage? {
        let appearance = NSApp?.effectiveAppearance ?? NSAppearance(named: .aqua)
        let dark = appearance?.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        let name = dark ? "OinkPasteMenuDark" : "OinkPasteMenu"
        return NSImage(named: name)
    }
    @objc private func pasteAsFile(_ sender: NSMenuItem) {
        // Finder proxies the menu across processes and sends a new NSMenuItem:
        // representedObject is nil in real Finder on macOS 26.6.2, even though
        // direct action tests retain it. Keep this menu's destination in the
        // extension instance; clipboard data and permissions are unaffected.
        guard let destination = menuDestination else { return }
        // NSWorkspace explicitly ignores argv from sandboxed callers. A bare
        // launch therefore consumes one-shot action requests from the existing
        // App Group; no clipboard bytes or file payloads use this mailbox.
        // https://developer.apple.com/documentation/appkit/nsworkspace/openconfiguration/arguments
        guard let inbox = pasteRequestDirectory() else { return }
        let request = inbox.appendingPathComponent(UUID().uuidString + ".json")
        do {
            try FileManager.default.createDirectory(at: inbox, withIntermediateDirectories: true)
            let value = PasteRequest(directory: destination.path, uptime: ProcessInfo.processInfo.systemUptime)
            try JSONEncoder().encode(value).write(to: request, options: .atomic)
        } catch { NSLog("Oink Paste as File request failed: %@", error.localizedDescription); return }
        let outer = Bundle.main.bundleURL.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let helper = outer.appendingPathComponent("Contents/Helpers/OinkPaste.app")
        let config = NSWorkspace.OpenConfiguration()
        config.createsNewApplicationInstance = true
        config.activates = false
        config.allowsRunningApplicationSubstitution = false
        NSWorkspace.shared.openApplication(at: helper, configuration: config) { _, error in
            if let error {
                try? FileManager.default.removeItem(at: request)
                NSLog("Oink Paste as File helper failed: %@", error.localizedDescription)
            }
        }
    }
}
