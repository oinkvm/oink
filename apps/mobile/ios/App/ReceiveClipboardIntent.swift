import AppIntents

// This file is compiled into the app target only (App/, not Shared/).
// Xcode extracts App Intents metadata from every target that compiles an
// AppIntent type. While this intent was in Shared/, OinkShare.appex carried
// its own Metadata.appintents declaring the same action identifier. With the
// app not running, linkd then picked the share extension to perform it and
// failed ("Unable to locate com.hachiwii.oink.share for the
// com.apple.share-services extension point", LNConnectionErrorDomain 1100),
// which Shortcuts shows as "couldn't communicate with the app". A running
// app was connected to directly, so only the cold case failed. The action
// must be declared by the app alone, which iOS launches in the background.

/// 一键复制: returns the group's latest shared text or image to Shortcuts.
/// The shortcut feeds it into Apple's Copy to Clipboard action, which writes
/// the pasteboard; iOS does not reliably accept a direct UIPasteboard write
/// from a background App Intent, so this intent never writes it.
/// Titles and descriptions are zh-Hans keys of Localizable.xcstrings, which holds only
/// zh-Hans until the language setting exists (see AppText.pick).
struct CopyLatestSharedClipboardIntent: AppIntent {
    static let title: LocalizedStringResource = "获取最新共享内容"
    static let description = IntentDescription(
        "获取其它设备最新共享的文字或图片，接着用「拷贝到剪贴板」放到本机剪贴板。支持文字和图片，文件请在传输页下载。",
        categoryName: "剪贴板",
        searchKeywords: ["复制", "剪贴板", "粘贴"],
        resultValueName: "最新共享内容")
    static let openAppWhenRun = false

    @available(iOS 26.0, *)
    static var supportedModes: IntentModes { .background }

    @MainActor
    func perform() async throws -> some IntentResult & ReturnsValue<IntentFile> {
        .result(value: try await LatestClip.shortcutFile(from: CoreSession.shared))
    }
}

extension CoreSession: LatestClipSession {}

struct OinkShortcuts: AppShortcutsProvider {
    /// Phrases are zh-Hans keys of AppShortcuts.xcstrings (zh-Hans only for now).
    static var appShortcuts: [AppShortcut] {
        AppShortcut(intent: CopyLatestSharedClipboardIntent(),
                    phrases: ["用\(.applicationName)获取最新共享内容",
                              "获取\(.applicationName)最新共享内容",
                              "\(.applicationName)最新共享内容"],
                    shortTitle: "最新共享内容", systemImageName: "doc.on.clipboard")
    }
    static let shortcutTileColor: ShortcutTileColor = .pink
}
