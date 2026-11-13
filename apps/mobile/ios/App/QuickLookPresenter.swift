import QuickLook
import SwiftUI
import UIKit

/// The system viewer (Quick Look) for the transfer page's pictures, videos and files (user
/// 2026-11-11): it opens on the tapped item and pages through a `QuickLookSequence` (the
/// conversation's, or one folder level's), zooming out of and back into the item's bubble.
/// Saving and sharing are Quick Look's own share button (its 存储图像 asks for the
/// add-only Photos permission itself).
@MainActor
final class QuickLookPresenter: NSObject, QLPreviewControllerDataSource, QLPreviewControllerDelegate {
    /// A sequence to page through: its items as opened, how each one reads now (a stand-in
    /// turns into its original once downloaded), and what to do when a stand-in is shown.
    struct Source {
        var entries: [QuickLookSequence.Entry]
        var resolve: (String) -> QuickLookSequence.Entry?
        var onStandInShown: (String) -> Void
    }

    /// The open folder page opens the files of its level that finish downloading.
    var folderOpener: ((IOSFileToOpen) -> Bool)?

    private weak var controller: QLPreviewController?
    private var source: Source?
    /// The items as opened, by message id; Quick Look's count and order stay as opened.
    private var ids: [String] = []
    /// The file each index was last handed to Quick Look as.
    private var handed: [Int: URL] = [:]
    private var single: Item?
    private var watch: Timer?
    private var lastIndex = -1

    /// Opens Quick Look on `messageID` in `source`; false when it is not in the sequence.
    @discardableResult
    func open(_ messageID: String, in source: Source) -> Bool {
        guard controller == nil, let index = QuickLookSequence.index(of: messageID, in: source.entries),
              let presenter = Self.topController() else { return false }
        self.source = source
        ids = source.entries.map(\.messageID)
        present(from: presenter, at: index)
        startWatching()
        return true
    }

    /// One file shown alone (a download whose message is not in a sequence).
    func open(file: IOSFileToOpen) {
        guard controller == nil, let presenter = Self.topController() else { return }
        single = Item(url: file.url, title: file.name, messageID: nil)
        present(from: presenter, at: 0)
    }

    private func present(from presenter: UIViewController, at index: Int) {
        handed = [:]
        lastIndex = -1
        let preview = QLPreviewController()
        preview.dataSource = self
        preview.delegate = self
        preview.currentPreviewItemIndex = index
        // Full screen (the default), not over the page: presented over the page, Quick Look
        // faded out on closing instead of zooming back into the bubble (integration
        // 2026-11-11; UIKit hands a custom dismissal the page's view only when the page left
        // the window). Taking the page out of the window no longer moves it: its appear work
        // runs once and its rows keep what is open.
        controller = preview
        presenter.present(preview, animated: true)
    }

    /// The items changed: one shown with its thumbnail that has downloaded since is shown
    /// again with the original, in place.
    func refresh() {
        guard let controller else { return }
        if let single {
            if let url = single.previewItemURL, !FileManager.default.fileExists(atPath: url.path) {
                controller.dismiss(animated: false)
                previewControllerDidDismiss(controller)
            }
            return
        }
        let index = controller.currentPreviewItemIndex
        guard let shown = handed[index] else { return }
        guard let entry = entry(at: index) else {
            // Quick Look keeps the already handed image even after the store purges it.
            // Dismiss without a transition snapshot when a password's backing file is gone.
            if !FileManager.default.fileExists(atPath: shown.path) {
                controller.dismiss(animated: false)
                previewControllerDidDismiss(controller)
            }
            return
        }
        if entry.url != shown { controller.refreshCurrentPreviewItem() }
    }

    private func entry(at index: Int) -> QuickLookSequence.Entry? {
        guard ids.indices.contains(index) else { return nil }
        return source?.resolve(ids[index])
    }

    // Quick Look has no callback for paging: its current index is read while it is shown
    // (KVO on currentPreviewItemIndex is not documented to fire on a swipe). Four times a
    // second is enough to start a stand-in's download.
    private func startWatching() {
        watch?.invalidate()
        watch = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.currentChanged() }
        }
    }

    private func currentChanged() {
        guard let controller else { return }
        let index = controller.currentPreviewItemIndex
        guard index != lastIndex else { return }
        lastIndex = index
        guard let entry = entry(at: index) else { return }
        if entry.standIn { source?.onStandInShown(entry.messageID) }
        refresh()
    }

    // MARK: QLPreviewControllerDataSource

    nonisolated func numberOfPreviewItems(in controller: QLPreviewController) -> Int {
        MainActor.assumeIsolated { single != nil ? 1 : ids.count }
    }

    nonisolated func previewController(_ controller: QLPreviewController, previewItemAt index: Int) -> QLPreviewItem {
        MainActor.assumeIsolated {
            if let single { return single }
            // An item gone meanwhile (deleted while open) shows nothing.
            guard let entry = entry(at: index) else { return Item(url: nil, title: nil, messageID: nil) }
            handed[index] = entry.url
            return Item(url: entry.url, title: entry.title, messageID: entry.messageID)
        }
    }

    // MARK: QLPreviewControllerDelegate

    /// The bubble the item zooms out of and back into, when it is on screen; otherwise
    /// Quick Look's own transition. Found by the item Quick Look asks about: on closing, the
    /// controller's current index no longer named the shown item (integration 2026-11-11:
    /// closing faded out instead of zooming back into the bubble).
    nonisolated func previewController(_ controller: QLPreviewController, transitionViewFor item: QLPreviewItem) -> UIView? {
        MainActor.assumeIsolated {
            let index = controller.currentPreviewItemIndex
            guard let id = (item as? Item)?.messageID ?? (ids.indices.contains(index) ? ids[index] : nil) else { return nil }
            return QuickLookAnchors.view(for: id)
        }
    }

    nonisolated func previewController(_ controller: QLPreviewController, editingModeFor previewItem: QLPreviewItem) -> QLPreviewItemEditingMode {
        // Received files are not marked up in place.
        .disabled
    }

    nonisolated func previewControllerDidDismiss(_ controller: QLPreviewController) {
        MainActor.assumeIsolated {
            guard self.controller === controller else { return }
            watch?.invalidate()
            watch = nil
            self.controller = nil
            source = nil
            single = nil
            ids = []
            handed = [:]
        }
    }

    /// The front view controller of the active scene (also the 一键复制 page's share sheet).
    static func topController() -> UIViewController? {
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        let scene = scenes.first { $0.activationState == .foregroundActive } ?? scenes.first
        var top = (scene?.windows.first { $0.isKeyWindow } ?? scene?.windows.first)?.rootViewController
        while let presented = top?.presentedViewController { top = presented }
        return top
    }

    private final class Item: NSObject, QLPreviewItem {
        let previewItemURL: URL?
        let previewItemTitle: String?
        /// The conversation's (or folder's) message it shows, for the zoom.
        let messageID: String?
        init(url: URL?, title: String?, messageID: String?) {
            previewItemURL = url
            previewItemTitle = title
            self.messageID = messageID
        }
    }
}

/// The bubbles Quick Look zooms from: an invisible UIView behind each picture and file
/// bubble, found by message id while it is on screen.
@MainActor
enum QuickLookAnchors {
    private final class Weak {
        weak var view: UIView?
        init(_ view: UIView) { self.view = view }
    }
    private static var views: [String: Weak] = [:]

    static func register(_ view: UIView, for id: String) { views[id] = Weak(view) }

    static func remove(_ view: UIView, for id: String) {
        if views[id]?.view === view { views[id] = nil }
    }

    /// The anchor of `id` if it is in a window and at least partly on screen.
    static func view(for id: String) -> UIView? {
        guard let view = views[id]?.view, let window = view.window else { return nil }
        let frame = view.convert(view.bounds, to: window)
        return frame.intersects(window.bounds) ? view : nil
    }
}

/// An item's bubble for Quick Look's zoom (QuickLookAnchors).
struct QuickLookAnchor: UIViewRepresentable {
    let id: String

    func makeUIView(context: Context) -> UIView {
        let view = UIView()
        view.isUserInteractionEnabled = false
        view.backgroundColor = .clear
        QuickLookAnchors.register(view, for: id)
        context.coordinator.id = id
        return view
    }

    func updateUIView(_ view: UIView, context: Context) {
        guard context.coordinator.id != id else { return }
        if let old = context.coordinator.id { QuickLookAnchors.remove(view, for: old) }
        QuickLookAnchors.register(view, for: id)
        context.coordinator.id = id
    }

    static func dismantleUIView(_ view: UIView, coordinator: Coordinator) {
        if let id = coordinator.id { QuickLookAnchors.remove(view, for: id) }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }
    final class Coordinator { var id: String? }
}
