import Foundation

/// 一键复制's ready-made shortcut (user 2026-11-12): `OinkCopy-<colour>.shortcut` in the
/// app bundle, signed for anyone by `apps/mobile/ios/Shortcuts/sign-shortcut.sh`
/// (it carries no Apple ID). Opening it shows Shortcuts' 添加快捷指令 sheet.
///
/// An imported shortcut cannot show Oink's app icon (see make_shortcut.py), so each
/// variant has the clipboard glyph on the Shortcuts tile colour closest to one theme
/// accent, and a tap imports the variant of the accent in use at that moment.
enum ShortcutFile {
    /// Theme accent (`OinkPalette.Accent` raw value) → tile colour of its variant.
    /// The same table as `ACCENTS` in make_shortcut.py (packaging/tests/test_ios_shortcut.py).
    static let colorByAccent: [String: String] = [
        "pink": "red",
        "orange": "darkOrange",
        "violet": "violet",
        "blue": "darkBlue",
        "teal": "teal",
    ]
    static let fileExtension = "shortcut"

    /// Bundle resource name of the variant for `accent`; nil for an accent without one.
    static func resource(accent: String) -> String? {
        colorByAccent[accent].map { "OinkCopy-\($0)" }
    }
    /// The shortcut's name: Shortcuts names an imported shortcut after its
    /// file. The same name as the page's texts (「Oink 复制」, mockup A).
    static var shortcutName: String { AppText.pick("Oink 复制", "Oink Copy") }

    /// Copies `source` to a fresh folder in `temporary` as
    /// `<shortcutName>.shortcut` and removes the copies of earlier taps
    /// (the share sheet or Shortcuts may still read the current one).
    static func stage(_ source: URL, in temporary: URL, name: String = shortcutName) throws -> URL {
        let parent = temporary.appendingPathComponent("OinkShortcut", isDirectory: true)
        let manager = FileManager.default
        if let earlier = try? manager.contentsOfDirectory(at: parent, includingPropertiesForKeys: nil) {
            for folder in earlier { try? manager.removeItem(at: folder) }
        }
        let folder = parent.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try manager.createDirectory(at: folder, withIntermediateDirectories: true)
        let file = folder.appendingPathComponent(name).appendingPathExtension(fileExtension)
        try manager.copyItem(at: source, to: file)
        return file
    }
}
