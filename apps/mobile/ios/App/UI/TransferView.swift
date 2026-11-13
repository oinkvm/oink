import Combine
import os
import SwiftUI
import UIKit
import QuickLook
import PhotosUI
import UniformTypeIdentifiers

/// 传输 tab (mockup `TransferPage.jsx`): the conversation, the composer with the proxy
/// computer, the full-screen image viewer and the folder page.
struct TransferView: View {
    /// Not observed: the page follows `source` (what it shows) and the session's
    /// transfer events (onReceive), not every change of the session.
    private let core = CoreSession.shared
    @StateObject private var source = TransferPageSource()
    @Environment(\.oink) private var oink
    @StateObject private var model = TransferModel()
    @StateObject private var toasts = ToastCenter()
    @State private var draft = ""
    /// The text whose send is still running (a second tap does not resend it).
    @State private var sendingText: String?
    /// Quick Look over the conversation's pictures, videos and files.
    @State private var quickLook = QuickLookPresenter()
    @State private var folder: TransferMessage?
    @State private var showFiles = false
    @State private var showPhotos = false
    @State private var showCamera = false
    @State private var photos: [PhotosPickerItem] = []
    @FocusState private var editing: Bool
    /// Typed with the row ids (undocumented behaviour, measured on the iOS 27 simulator):
    /// with an untyped `ScrollPosition(edge:)` every touch-down on the list made the lazy
    /// stack build and measure the last rows of the conversation (about 10) and drop them
    /// again, wherever the list was: a 20 ms main-thread pass at the start of each scroll
    /// on a 400-record conversation. Typed, SwiftUI keeps the position as the id of a row
    /// on screen and touch-down builds nothing.
    @State private var scroll = ScrollPosition(idType: String.self, edge: .bottom)
    /// Shown lines of long texts opened with 查看更多; a text without an entry is folded.
    @State private var unfolded: [String: Int] = [:]
    /// Sensitive texts shown with 显示 (hidden again like a fold, SPEC §1.5).
    @State private var revealed: Set<String> = []
    /// Batches shown item by item (TransferLayout.batchKey).
    @State private var expanded: Set<String> = []
    /// Top card index of each collapsed batch (0 = its first item).
    @State private var tops: [String: Int] = [:]
    /// Scrolling and the fully visible batch items (for 收起). Held in @State, not
    /// @StateObject, so this page does not observe it; only the chips do.
    @State private var collapseState = CollapseState()
    /// The text message in system text selection (长按「选择」), and its frame in the list.
    @State private var selecting: String?
    @State private var selectionFrame: CGRect = .zero
    /// The running expand / collapse (stand-ins for the batch's items over the list) and
    /// its time in ms, animated linearly.
    @State private var morph: BatchMorph?
    @State private var morphTime: Double = 0
    /// While a batch expands or collapses, the rows around it slide from where they were.
    @State private var rowShift = RowShift()
    /// The batch whose 展开 fades in after it collapsed.
    @State private var pillEnter: String?
    /// Room kept at the list's end while a collapse switches its layout (switchLayout).
    @State private var tailSpace: CGFloat = 0
    /// The list's content offset, read without re-rendering (expand / collapse keep the batch in place).
    @State private var book = ScrollBook()
    @State private var settle: Task<Void, Never>?
    /// DEBUG design state transfer-stack-swipe: the finger 50 pt to the left.
    @State private var stackDemoDrag: CGFloat?
    /// The page's and the list's first appearance happened (their appear work runs once).
    @State private var started = false
    @State private var listShown = false
    /// Width of the conversation (page margins and the widest message body).
    @State private var listWidth: CGFloat = 0
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    static let listSpace = "transfer-list"
    static let contentSpace = "transfer-content"

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                SessionBanner()
                messages
                composer
            }
            .background(oink.bg)
            .overlay(alignment: .topTrailing) { designControls }
            .overlay(alignment: .top) {
                if let toast = toasts.toast {
                    Text(toast.text)
                        .font(.oink(12)).foregroundStyle(oink.toastInk)
                        .padding(.horizontal, 16).padding(.vertical, 11)
                        .background(oink.toast, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
                        .shadow(color: oink.shadow, radius: 8, y: 4)
                        .padding(.top, 8)
                        .transition(.opacity)
                }
            }
            .toolbar(.hidden, for: .navigationBar)
            .navigationDestination(item: $folder) { message in
                FolderPage(message: message, model: model, quickLook: quickLook).toolbar(.hidden, for: .navigationBar)
            }
        }
        .task {
            // Once per page, not on each appearance: coming back from the folder page (or
            // another tab, or a cover) appears again, and redoing this moved the list.
            guard !started else { return }
            started = true
            #if DEBUG
            if let scene = DesignScene.current, scene.hasPrefix("transfer") || scene.hasPrefix("share") {
                // The last choice before the devices appear, so the composer starts from it.
                core.setTransferProxy(Self.designChoice(scene))
                core.installUIFixtureIfRequested()
                applyDesignScene(scene)
                FrameProbe.startIfRequested()
                return
            }
            #endif
            model.loadInitial()
        }
        // The model follows the session's record changes and restarts itself (TransferModel.init).
        .onReceive(core.$transferError.compactMap { $0 }.receive(on: DispatchQueue.main)) { error in
            toasts.show(error)
            core.transferError = nil
        }
        .onDisappear {
            // Leaving the page must not open a file the user no longer waits for.
            core.cancelTransferAutoOpen()
            // Leaving the page (another tab) ends editing: the page opens with the keyboard
            // closed and only a tap on the composer opens it (用户 2026-11-12). Under another
            // tab the field is out of the window and loses first responder, but `editing`
            // stayed true, and SwiftUI focused the field again as soon as the page was back
            // in the window — the keyboard came up on every return to the tab. Going to the
            // background is not leaving the page: the app comes back as it was left.
            editing = false
        }
        // A download opened by a tap finished: Quick Look on it, paging through the
        // conversation when it is a message of it, or through the open folder's level.
        .onChange(of: source.fileToOpen) { _, file in
            guard let file else { return }
            core.fileToOpen = nil
            if let message = model.messages.first(where: { $0.recordID == file.id }), quickLook.open(message.id, in: quickLookSource) {
                return
            }
            if quickLook.folderOpener?(file) == true { return }
            quickLook.open(file: file)
        }
        .onChange(of: model.messages) { _, _ in quickLook.refresh() }
        // A purged auto-opened file may not have been in the currently loaded list.
        .onChange(of: core.transferChange) { _, _ in quickLook.refresh() }
        .alert("Oink", isPresented: Binding(get: { !model.errorMessage.isEmpty }, set: { if !$0 { model.errorMessage = "" } })) {
            Button("好") { model.errorMessage = "" }
        } message: { Text(model.errorMessage) }
        .fileImporter(isPresented: $showFiles, allowedContentTypes: [.item, .folder], allowsMultipleSelection: true) { result in
            switch result {
            case .success(let urls): send(files: urls, scoped: true)
            case .failure(let error): model.errorMessage = error.localizedDescription
            }
        }
        // Several photos at once go out as one share (one batch on every device).
        .photosPicker(isPresented: $showPhotos, selection: $photos, maxSelectionCount: nil, selectionBehavior: .ordered,
                      matching: .images)
        .onChange(of: photos) { _, items in
            guard !items.isEmpty else { return }
            Task { @MainActor in
                var urls: [URL] = []
                do {
                    for item in items {
                        guard let data = try await item.loadTransferable(type: Data.self) else { continue }
                        // Keep the real type (HEIC, PNG, ...): the core sends it
                        // with the mime derived from this extension.
                        let ext = item.supportedContentTypes.first?.preferredFilenameExtension ?? "jpg"
                        let url = FileManager.default.temporaryDirectory.appendingPathComponent("oink-share-\(UUID().uuidString).\(ext)")
                        try data.write(to: url)
                        urls.append(url)
                    }
                    if !urls.isEmpty { send(files: urls, temporary: true) }
                } catch { model.errorMessage = error.localizedDescription }
                photos = []
            }
        }
    }

    // MARK: Conversation

    private var ordered: [TransferMessage] { Array(model.messages.reversed()) }

    private var messages: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                if model.olderLoadFailed {
                    VStack(spacing: 8) {
                        Text(AppText.pick("加载失败", "Loading failed")).font(.oink(11)).foregroundStyle(oink.hint)
                        Button(AppText.pick("加载更早的记录", "Load earlier records")) { model.loadMore() }.font(.oink(12)).tint(oink.accentInk)
                    }.frame(maxWidth: .infinity).padding(.vertical, 16)
                } else if model.hasOlder {
                    HStack(spacing: 6) { ProgressView().controlSize(.mini); Text(AppText.pick("正在加载更早的记录…", "Loading earlier records…")).font(.oink(11)).foregroundStyle(oink.hint) }
                        .frame(maxWidth: .infinity).padding(.vertical, 12)
                        .onAppear { if DesignScene.current == nil { model.loadMore() } }
                }
                if let boundary=model.sharedBoundary, let oldest=ordered.first, oldest.createdAt < boundary {
                    Text(AppText.pick("更早的记录仅保存在本机", "Earlier records are stored only on this phone"))
                        .font(.oink(10)).foregroundStyle(oink.hint).frame(maxWidth:.infinity).padding(.vertical,12)
                }
                Text(dateLabel).font(.oink(10)).kerning(0.3).foregroundStyle(oink.hint)
                    .frame(maxWidth: .infinity).frame(height: 14)
                    // The list's own 4 pt top margin, inside the scroll target layout (below).
                    .padding(.top, 2 + 4).padding(.bottom, 24)
                    .onAppear { if DesignScene.current == nil, !model.olderLoadFailed, !model.isLoading { model.loadMore() } }
                let entries = TransferLayout.entries(ordered, expanded: expanded)
                let batchRows = morph.map { morph in entries.indices.filter { Self.batch(of: entries[$0]) == morph.batch } } ?? []
                ForEach(Array(entries.enumerated()), id: \.element.id) { index, entry in
                    if let boundary=model.sharedBoundary, entry.last.createdAt >= boundary,
                       index == 0 || entries[index-1].last.createdAt < boundary {
                        VStack(spacing:8) { Text(AppText.pick("已显示最近 100 条共享记录", "Showing the latest 100 shared records")); Divider(); Text(AppText.pick("更早的本机记录", "Earlier local records")) }
                            .font(.oink(10)).foregroundStyle(oink.hint).padding(.vertical,16).frame(maxWidth:.infinity)
                    }
                    let continuation = isContinuation(entries, index)
                    row(entry, index: index, continuation: continuation, hidden: batchRows.contains(index))
                        .padding(.horizontal, TransferLayout.listMargin(width: listWidth))
                        .padding(.top, continuation ? -14 : 0)
                        .padding(.bottom, 24)
                        // Batch rows and their neighbours report where they are: the rows around a
                        // morph slide by how far the neighbours moved.
                        .modifier(FrameReport(report: Self.nearBatch(entries, index) ? { [book] in book.frames[ScrollBook.rowKey(entry.id)] = $0 } : nil))
                        .offset(y: rowShift.offset(index: index, batch: batchRows))
                        .id(entry.id)
                }
                if tailSpace > 0 { Color.clear.frame(height: tailSpace) }
            }
            // Lets the scroll position follow the rows, so a long text folding
            // again (or a batch changing) off screen does not move what is shown.
            // The margins are inside the rows, so the target layout starts at the content's
            // origin and its rows span its width: with the 20 pt margins around it, the
            // position kept on a row (or set as a point) put the content 20 pt to the left
            // after a batch expanded, until the next scroll (integration 2026-11-11).
            .scrollTargetLayout()
            // The content's own coordinates (top 0 = scroll offset 0): where opened rows are.
            .coordinateSpace(.named(Self.contentSpace))
            .padding(.bottom, -6)
        }
        .coordinateSpace(.named(Self.listSpace))
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { listWidth = $0 }
        .defaultScrollAnchor(.bottom, for: .initialOffset)
        .scrollPosition($scroll)
        // Older pages append to the newest-first model. Keep the user's scroll position there;
        // follow the bottom only when the newest message changes.
        .onChange(of: model.messages.first?.id) { _, next in if next != nil { scroll.scrollTo(edge: .bottom) } }
        // The first time the list shows, it ends at the newest message; coming back from the
        // folder page (a pushed page makes the list disappear and appear again) it stays
        // where the user left it.
        .task {
            guard !listShown else { return }
            listShown = true
            try? await Task.sleep(for: .milliseconds(200))
            scroll.scrollTo(edge: .bottom)
        }
        .onScrollGeometryChange(for: ListGeometry.self) { geometry in
            let content = geometry.contentSize.height + geometry.contentInsets.top + geometry.contentInsets.bottom
            // In the scroll position's coordinates (scrollTo(point:) / (y:)), which start at the
            // content's top, not at UIKit's content offset (that is less by the top inset): a
            // morph pinned with the raw offset moved the list by the inset (68 pt here).
            return ListGeometry(offset: geometry.contentOffset.y + geometry.contentInsets.top, content: content,
                                viewport: geometry.containerSize.height)
        } action: { _, value in
            book.offset = value.offset
            book.content = value.content
            book.viewport = value.viewport
            book.maxOffset = max(0, value.content - value.viewport)
            foldFarRows()
        }
        // The batch's stand-ins while it expands or collapses, over the list (its viewport
        // is the list space the rows report their frames in).
        .overlay(alignment: .topLeading) {
            if let morph {
                ZStack(alignment: .topLeading) {
                    ForEach(morph.ghosts) { ghost in
                        MorphGhost(time: morphTime, ghost: ghost, mine: morph.mine)
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
                .clipped()
                .allowsHitTesting(false)
            }
        }

        // 收起 hides as soon as the list scrolls and returns 80 ms after it rests (user 2026-11-11;
        // SPEC §3.1 had 220 ms). `.idle` comes only once the deceleration has ended.
        .onScrollPhaseChange { _, phase in
            settle?.cancel()
            if phase != .idle {
                collapseState.setScrolling(true)
            } else {
                settle = Task { @MainActor in
                    try? await Task.sleep(for: .milliseconds(80))
                    if !Task.isCancelled { collapseState.setScrolling(false) }
                }
            }
        }
        .scrollIndicators(.hidden)
        .scrollDismissesKeyboard(.interactively)
        // A tap anywhere in the conversation ends editing and a text selection
        // (unless it lands on the selected text); controls in the bubbles still fire.
        .simultaneousGesture(SpatialTapGesture(coordinateSpace: .named(Self.listSpace)).onEnded { tap in
            editing = false
            if selecting != nil, !selectionFrame.contains(tap.location) { selecting = nil }
        })
    }

    /// A batch's row or a row next to one (they report where they are, for the morph).
    static func nearBatch(_ entries: [TransferEntry], _ index: Int) -> Bool {
        batch(of: entries[index]) != nil || (index > 0 && batch(of: entries[index - 1]) != nil)
            || (index + 1 < entries.count && batch(of: entries[index + 1]) != nil)
    }

    /// The batch a row belongs to (a stack, or an item of an expanded batch).
    static func batch(of entry: TransferEntry) -> String? {
        switch entry {
        case .message(_, let batch): batch
        case .stack(let key, _): key
        }
    }

    @ViewBuilder private func row(_ entry: TransferEntry, index: Int, continuation: Bool, hidden: Bool) -> some View {
        switch entry {
        case .message(let message, let batch):
            messageRow(message, index: index, continuation: continuation, batch: batch, hidden: hidden)
        case .stack(let key, let items):
            let top = min(tops[key] ?? 0, items.count - 1)
            let item = items[top]
            let book = book
            MessageRow(message: Self.stackHead(item, items), continuation: continuation, sender: sender(of: item), proxy: proxy(of: item),
                       progress: progress(of: item), reduceMotion: reduceMotion,
                       bodyWidth: TransferLayout.bodyMaxWidth(width: listWidth), hidden: hidden,
                       stack: .init(items: items, top: top, demoDrag: key == "fixture-macMini/fixture-op-a" ? stackDemoDrag : nil,
                                    progress: items.map(progress(of:)), enter: pillEnter == key,
                                    onMove: { tops[key] = $0 }, onExpand: { expand(key, items: items, top: top) },
                                    onEntered: { if pillEnter == key { pillEnter = nil } },
                                    onBoxFrame: { book.frames[ScrollBook.boxKey(key)] = $0 }),
                       onOpenImage: { openImage(item) },
                       onImageVisible: { items.forEach(model.requestThumbnail) },
                       onOpenFile: { open(item) },
                       onDownload: { download(item) },
                       // A collapsed stack offers no 复制 (TransferLayout.offersCopy).
                       onCopy: {},
                       onDelete: { model.delete(item) },
                       peerName: peerName(of: item),
                       onCancelTransfer: { cancelTransfer(Self.stackHead(item, items)) })
                .equatable()
        }
    }

    /// `index`: the row's place in the conversation (orders the fully visible batch items for 收起).
    private func messageRow(_ message: TransferMessage, index: Int, continuation: Bool, batch: String?, hidden: Bool) -> some View {
        let id = message.id
        let book = book
        let lines = unfolded[id] ?? TransferLayout.foldedLines
        let batchItems = batch.map { key in ordered.filter { TransferLayout.batchKey($0) == key } } ?? []
        // The batch is read when 收起 is tapped: an unchanged row keeps its old closures (MessageRow ==).
        let onCollapse: (() -> Void)? = batch.map { key in {
            let items = ordered.filter { TransferLayout.batchKey($0) == key }
            collapse(key, items: items, at: items.firstIndex { $0.id == id } ?? 0)
        } }
        let selected = selecting == id
        let opened = selected || unfolded[id] != nil || revealed.contains(id)
        // In an expanded batch only the last item shows the batch's send state.
        var shown = message
        if batch != nil {
            if batchItems.last?.id == id { (shown.delivery, shown.deliveryFailure) = TransferLayout.delivery(batchItems) }
            else if !message.isDirect || message.delivery != .sent { shown.delivery = nil }
        }
        return MessageRow(message: shown, continuation: continuation, sender: sender(of: message),
                          proxy: proxy(of: message), progress: progress(of: message), reduceMotion: reduceMotion,
                          bodyWidth: TransferLayout.bodyMaxWidth(width: listWidth), hidden: hidden,
                          inBatch: batch != nil,
                          onBubbleFrame: batch == nil ? nil : { book.frames[id] = $0 },
                          lines: lines, onMore: {
                              withAnimation(reduceMotion ? nil : TransferMotion.text) { unfolded[id] = lines + TransferLayout.moreLines }
                          },
                          selecting: selecting == id,
                          onSelect: { editing = false; selecting = id },
                          onEndSelect: { if selecting == id { selecting = nil } },
                          revealed: revealed.contains(id),
                          onReveal: { show in reveal(message, show) },
                          onCollapse: onCollapse, collapseState: collapseState,
                          onOpenImage: { openImage(message) },
                          onImageVisible: { model.requestThumbnail(message) },
                          onOpenFile: { open(message) },
                          onDownload: { download(message) },
                          onCopy: { copy(message) },
                          onCopyText: { copy(message, rich: false) },
                          onDelete: { model.delete(message) },
                          peerName: peerName(of: message),
                          onCancelTransfer: { cancelTransfer(shown) },
                          rich: model.richContent(message),
                          index: index, opened: opened,
                          tracking: .init(
                              // Where the selected text is, so a tap on it keeps the selection.
                              selectionFrame: { frame in if selecting == id { selectionFrame = frame } },
                              // An unfolded long text folds again (and a selection ends) once it is
                              // more than half a screen out of view (§1.5), by how far the list
                              // scrolled: while the row is laid out from its own place, after the
                              // lazy list dropped it from where it was last (foldFarRows). Folding
                              // above the view keeps the list still.
                              side: { side in
                                  book.sides[id] = side
                                  if abs(side) == 2 { reset(id) }
                              },
                              place: { frame in book.opened[id] = frame.isNull ? nil : (top: frame.minY, height: frame.height) },
                              height: { height in compensate(id, height: height) },
                              // A row going away is not a reason to fold or end a selection: the
                              // lazy list drops rows, and the whole page goes away under the folder
                              // page, another tab or a cover; the page keeps what is open.
                              gone: { collapseState.setFullyVisible(id, at: nil) },
                              fullyVisible: { inside in collapseState.setFullyVisible(id, at: inside ? index : nil) }))
            .equatable()
    }

    /// The other phone of a phone-to-phone record, by its current name.
    private func peerName(of message: TransferMessage) -> String? {
        guard let id = message.directPeerID else { return nil }
        if !message.isMine { return source.cards?.first { $0.isSelf }?.name ?? AppText.pick("本机", "This phone") }
        return source.targetNames[id] ?? card(id)?.name ?? message.directPeerName ?? id
    }

    /// Long-press「取消发送」/「取消接收」(review 2026-11-11). Cancelling a send removes the
    /// message on both sides; cancelling a receive leaves「已取消」on both. No retry.
    private func cancelTransfer(_ message: TransferMessage) {
        guard let kind = TransferLayout.inFlight(message) else { return }
        Haptics.selection()
        #if DEBUG
        if DesignScene.hasPrefix("transfer") { model.fixtureCancel(message, sending: kind == .sending); return }
        #endif
        do {
            switch kind {
            case .sending: try core.cancelTransferSend(message)
            case .receiving: try core.cancelTransferReceive(message)
            }
        } catch { model.errorMessage = error.oinkMessage }
    }

    /// 显示 / 隐藏 of a sensitive text. A secret is never fetched on its own:
    /// showing it downloads the body when it is not here yet.
    private func reveal(_ message: TransferMessage, _ show: Bool) {
        Haptics.selection()
        if show {
            revealed.insert(message.id)
            if message.text == nil, message.state != .downloading { model.download(message, open: false) }
        } else {
            revealed.remove(message.id)
            unfolded[message.id] = nil
        }
    }

    /// The top card's record with the send state of the whole batch.
    private static func stackHead(_ item: TransferMessage, _ items: [TransferMessage]) -> TransferMessage {
        var head = item
        (head.delivery, head.deliveryFailure) = TransferLayout.delivery(items)
        return head
    }

    private func reset(_ id: String) {
        book.opened[id] = nil
        if unfolded[id] != nil || revealed.contains(id) {
            // Above the view the fold shortens the content over what is shown:
            // remember the height so the list can be moved by the difference.
            if (book.sides[id] ?? 0) < 0 { book.pending[id] = (book.heights[id] ?? 0, book.offset) }
            unfolded[id] = nil
            revealed.remove(id)
        }
        if selecting == id { selecting = nil }
    }

    /// Opened rows (unfolded, shown or selected) the list has scrolled more than half a screen
    /// away from, laid out or not, fold again (TransferView.side for the rule).
    private func foldFarRows() {
        guard !book.opened.isEmpty, book.viewport > 0 else { return }
        let margin = book.viewport / 2
        for (id, place) in book.opened {
            let top = place.top - book.offset
            if top + place.height < -margin {
                book.sides[id] = -2
                reset(id)
            } else if top > book.viewport + margin {
                book.sides[id] = 2
                reset(id)
            }
        }
    }

    /// A row's height changed: after a fold above the view, move the list by the
    /// difference unless the scroll view already kept the content in place.
    private func compensate(_ id: String, height: CGFloat) {
        guard let before = book.pending.removeValue(forKey: id) else { book.heights[id] = height; return }
        book.heights[id] = height
        Task { @MainActor in
            // After the scroll view's own update of this layout pass.
            await Task.yield()
            let moved = book.offset - before.offset
            let delta = height - before.height
            if abs(delta - moved) > 1 { keepInPlace(delta - moved) }
        }
    }

    /// Where a row is against the visible list: -2 / 2 more than half a screen
    /// above / below, -1 / 1 just above / below, 0 at least partly visible.
    static func side(_ proxy: GeometryProxy) -> Int {
        guard let visible = proxy.bounds(of: .scrollView) else { return 0 }
        let margin = visible.height / 2
        if proxy.size.height < visible.minY { return proxy.size.height < visible.minY - margin ? -2 : -1 }
        if 0 > visible.maxY { return 0 > visible.maxY + margin ? 2 : 1 }
        return 0
    }

    static func fullyInView(_ proxy: GeometryProxy) -> Bool {
        guard let visible = proxy.bounds(of: .scrollView) else { return false }
        return visible.minY <= 0.5 && proxy.size.height <= visible.maxY + 0.5
    }

    /// Height of a batch item as a normal message (for keeping the batch in place).
    private func expandedHeight(_ message: TransferMessage) -> Double {
        guard message.kind == .image else { return 70 }
        return TransferLayout.fit(aspect: MessageRow.picture(message, progress: progress(of: message))?.aspect ?? 3.0 / 4,
                                  in: TransferLayout.expandedImageBox).height
    }

    /// An item's card in the stack: its picture contained in 120 × 160, files 3:4.
    private func cardSize(_ item: TransferMessage) -> CGSize {
        guard item.kind == .image else { return TransferLayout.stackBox }
        return TransferLayout.fit(aspect: MessageRow.picture(item, progress: progress(of: item))?.aspect ?? 3.0 / 4,
                                  in: TransferLayout.stackBox)
    }

    /// An item's message when it is not laid out: pictures in 220 × 280, files 220 × 70.
    private func bubbleSize(_ item: TransferMessage) -> CGSize {
        guard item.kind == .image else { return CGSize(width: 220, height: 70) }
        return TransferLayout.fit(aspect: MessageRow.picture(item, progress: progress(of: item))?.aspect ?? 3.0 / 4,
                                  in: TransferLayout.expandedImageBox)
    }

    /// The batch's stand-ins, farthest from the top card first (it is drawn last, on top).
    private func ghostPlans(_ items: [TransferMessage], anchor: Int) -> [MorphGhostPlan] {
        items.indices.sorted { TransferLayout.ringDistance(items.count, $0, anchor) > TransferLayout.ringDistance(items.count, $1, anchor) }
            .map { index in
                let item = items[index]
                return MorphGhostPlan(message: item, picture: MessageRow.picture(item, progress: progress(of: item)),
                                      progress: progress(of: item), detail: MessageRow.stackDetail(item),
                                      delay: TransferLayout.morphDelay(count: items.count, index: index, anchor: anchor))
            }
    }

    /// 展开 (Android BatchMorph): each item morphs from its card (or from the stack's
    /// middle) to its message, items laid out off screen fly out over the edge, the list
    /// moves so the top card's message lands where the stack box was, and the rows around
    /// slide from where they were.
    private func expand(_ key: String, items: [TransferMessage], top: Int) {
        guard morph == nil else { return }
        Haptics.selection()
        let heights = items.map(expandedHeight)
        let growth = TransferLayout.batchGrowth(heights: heights)
        guard !reduceMotion, let box = book.frames[ScrollBook.boxKey(key)] else {
            withAnimation(reduceMotion ? TransferMotion.reduced : nil) { _ = expanded.insert(key) }
            keepInPlace(TransferLayout.expandShift(heights: heights, top: top), maxOffset: book.maxOffset + growth)
            return
        }
        let roles = TransferLayout.roles(count: items.count, top: top)
        var plan = BatchMorph(batch: key, expanding: true, anchor: top, mine: items[top].isMine, ghosts: ghostPlans(items, anchor: top))
        for g in plan.ghosts.indices {
            let index = items.firstIndex { $0.id == plan.ghosts[g].message.id } ?? 0
            if let role = roles[index], role.pose.opacity > 0 {
                plan.ghosts[g].hasCard = true
                plan.ghosts[g].from = TransferLayout.cardGeo(box: box.origin, size: cardSize(items[index]), pose: role.pose)
            }
            plan.ghosts[g].hasBubble = true
            plan.ghosts[g].toBubble = true
        }
        let around = neighbours(of: key)
        morphLog("expand \(key) top \(top) offset \(book.offset) max \(book.maxOffset) growth \(growth) box \(box) around \(String(describing: around.before))")
        // 1. The new layout with the list where it is: nothing above the batch moves, the rows
        //    below are drawn where they were (by the estimated growth until measured).
        switchLayout(plan, below: -growth) { _ = expanded.insert(key) }
        Task { @MainActor in
            await DisplayFrame.next(2)
            guard morph?.batch == key else { return }
            // 2. One move: the top card's message to where the box was (estimated when that
            //    message is not laid out yet); the rows around keep where they were.
            let shift = book.frames[items[top].id].map { Double($0.minY - box.minY) }
                ?? TransferLayout.expandShift(heights: heights, top: top)
            move(by: shift, around: around, growth: growth)
            await DisplayFrame.next(2)
            guard var plan = morph, plan.batch == key, plan.expanding else { return }
            // 3. The messages where they are now (laid out after the move): the stand-ins'
            //    targets. The first row is drawn with the rows above's offset.
            let height = book.viewport
            let header = rowShift.above
            func rect(_ index: Int) -> CGRect? {
                book.frames[items[index].id].map { index == 0 ? $0.offsetBy(dx: 0, dy: -header) : $0 }
            }
            let topRect = rect(top)
            for g in plan.ghosts.indices {
                let index = items.firstIndex { $0.id == plan.ghosts[g].message.id } ?? 0
                let r = rect(index)
                if let r { plan.ghosts[g].bubble = r.size }
                if let r, r.maxY > -40, r.minY < height + 40 {
                    if plan.ghosts[g].from == nil {
                        plan.ghosts[g].from = TransferLayout.hiddenGeo(box: box.origin, size: cardSize(items[index]), card: false)
                    }
                    plan.ghosts[g].to = TransferLayout.MorphGeo(rect: r, card: false)
                } else if let from = plan.ghosts[g].from {
                    let size = r?.size ?? bubbleSize(items[index])
                    if plan.ghosts[g].bubble == nil { plan.ghosts[g].bubble = size }
                    let x = topRect.map { plan.mine ? $0.maxX - size.width : $0.minX } ?? from.rect.minX
                    let above = r.map { $0.minY < 0 } ?? (index < top)
                    plan.ghosts[g].to = TransferLayout.MorphGeo(
                        rect: CGRect(x: x, y: above ? -size.height - 24 : height + 24, width: size.width, height: size.height),
                        opacity: 0, card: false)
                }
            }
            start(plan)
        }
    }

    /// 收起 on an item (Android BatchMorph): the batch stacks again with that item on top,
    /// the stack box where that item's message was; the messages on screen morph into their
    /// cards, cards not on screen fade in at their places.
    private func collapse(_ key: String, items: [TransferMessage], at index: Int) {
        guard morph == nil else { return }
        Haptics.selection()
        let heights = items.map(expandedHeight)
        let growth = TransferLayout.batchGrowth(heights: heights)
        guard !reduceMotion, let tapped = book.frames[items[index].id] else {
            tops[key] = index
            withAnimation(reduceMotion ? TransferMotion.reduced : nil) { _ = expanded.remove(key) }
            keepInPlace(-TransferLayout.expandShift(heights: heights, top: index), maxOffset: book.maxOffset - growth)
            return
        }
        let height = book.viewport
        let roles = TransferLayout.roles(count: items.count, top: index)
        var plan = BatchMorph(batch: key, expanding: false, anchor: index, mine: items[index].isMine, ghosts: [])
        for var ghost in ghostPlans(items, anchor: index) {
            let i = items.firstIndex { $0.id == ghost.message.id } ?? 0
            let r = book.frames[ghost.message.id]
            let onScreen = r.map { $0.maxY > 0 && $0.minY < height } ?? false
            let inStack = roles[i].map { $0.pose.opacity > 0 } ?? false
            guard onScreen || inStack else { continue }
            ghost.hasCard = inStack
            ghost.hasBubble = onScreen
            ghost.toBubble = false
            if onScreen, let r {
                ghost.from = TransferLayout.MorphGeo(rect: r, card: false)
                ghost.bubble = r.size
            }
            plan.ghosts.append(ghost)
        }
        let around = neighbours(of: key)
        morphLog("collapse \(key) at \(index) offset \(book.offset) max \(book.maxOffset) growth \(growth) tapped \(tapped) around \(String(describing: around.before))")
        // 1. The new layout, keeping the list's height for it (a spacer for what the batch
        //    gives back) so the offset stays valid and nothing above the batch moves.
        switchLayout(plan, below: growth, tail: growth) {
            tops[key] = index
            // The stack's 展开 fades in once the morph is over.
            pillEnter = key
            _ = expanded.remove(key)
        }
        Task { @MainActor in
            await DisplayFrame.next(2)
            guard morph?.batch == key else { return }
            // 2. One move: the stack box to the tapped message. When the stack is not laid out
            //    (it went above the view), where it is follows from the tapped message.
            let shift = book.frames[ScrollBook.boxKey(key)].map { Double($0.minY - tapped.minY) }
                ?? -TransferLayout.expandShift(heights: heights, top: index)
            move(by: shift, around: around, growth: -growth)
            await DisplayFrame.next(2)
            guard var plan = morph, plan.batch == key, !plan.expanding else { return }
            // 3. The cards around the box where it is now: the stand-ins' targets. The stack's
            //    row is drawn with the rows above's offset.
            if let box = book.frames[ScrollBook.boxKey(key)]?.offsetBy(dx: 0, dy: -rowShift.above) {
                for g in plan.ghosts.indices {
                    let i = items.firstIndex { $0.id == plan.ghosts[g].message.id } ?? 0
                    let to = roles[i].flatMap { role in
                        role.pose.opacity > 0 ? TransferLayout.cardGeo(box: box.origin, size: cardSize(items[i]), pose: role.pose) : nil
                    }
                    if let from = plan.ghosts[g].from {
                        plan.ghosts[g].to = to ?? TransferLayout.hiddenGeo(box: box.origin, size: cardSize(items[i]), card: from.card)
                    } else if let to {
                        var appear = to
                        appear.opacity = 0
                        appear.scale *= 0.94
                        plan.ghosts[g].from = appear
                        plan.ghosts[g].to = to
                    }
                }
            }
            start(plan)
        }
    }

    /// The rows next to a batch (before and after it) and where they are now: the rows
    /// around a morph slide by how far these moved.
    private func neighbours(of batch: String) -> (ids: (prev: String?, next: String?), before: (prev: CGFloat?, next: CGFloat?)) {
        let entries = TransferLayout.entries(ordered, expanded: expanded)
        let rows = entries.indices.filter { Self.batch(of: entries[$0]) == batch }
        guard let first = rows.first, let last = rows.last else { return ((nil, nil), (nil, nil)) }
        let prev = first > 0 ? entries[first - 1].id : nil
        let next = last + 1 < entries.count ? entries[last + 1].id : nil
        return ((prev, next), (prev.flatMap { book.frames[ScrollBook.rowKey($0)]?.minY },
                               next.flatMap { book.frames[ScrollBook.rowKey($0)]?.minY }))
    }

    /// The first step of a morph: the batch's rows switch (hidden behind the stand-ins) with
    /// the list pinned at its offset as a point. Before, the switch and the move were one
    /// update with `scrollTo(y:)` (integration 2026-11-11, iOS 27 simulator): the target was
    /// cut at the old layout's end, and the position, otherwise kept on a row id, moved the
    /// list by itself on the switch. Now the move comes once the new layout is measured, and
    /// the stand-ins' targets once the move is laid out.
    private func switchLayout(_ plan: BatchMorph, below: Double, tail: Double = 0, change: () -> Void) {
        var still = Transaction()
        still.disablesAnimations = true
        withTransaction(still) {
            scroll.scrollTo(point: CGPoint(x: 0, y: book.offset))
            morph = plan
            morphTime = 0
            collapseState.setMorphing(true)
            tailSpace = tail
            change()
            rowShift = RowShift(above: 0, below: below)
        }
    }

    /// The second step: scrolls the switched layout by `shift` (cut at its ends) and draws the
    /// rows around the batch where they were before the switch, measured on its neighbour
    /// rows (or by the scroll, and the estimated growth, when they are not laid out). One
    /// update, without animation; the collapse's spacer goes in it.
    private func move(by shift: Double, around: (ids: (prev: String?, next: String?), before: (prev: CGFloat?, next: CGFloat?)),
                      growth: Double) {
        let applied = rowShift
        // Where the neighbours are in the switched layout, without the offsets drawn on them.
        let prevNow = around.ids.prev.flatMap { book.frames[ScrollBook.rowKey($0)]?.minY }.map { $0 - applied.above }
        let nextNow = around.ids.next.flatMap { book.frames[ScrollBook.rowKey($0)]?.minY }.map { $0 - applied.below }
        let end = max(0, book.maxOffset - tailSpace)
        let target = min(max(0, book.offset + shift), end)
        let delta = target - book.offset
        var above = delta
        if let old = around.before.prev, let now = prevNow { above = old - (now - delta) }
        var below = above - growth
        if let old = around.before.next, let now = nextNow { below = old - (now - delta) }
        morphLog("move offset \(book.offset) shift \(shift) delta \(delta) end \(end) prev \(String(describing: prevNow)) next \(String(describing: nextNow)) -> \(above),\(below)")
        var still = Transaction()
        still.disablesAnimations = true
        withTransaction(still) {
            tailSpace = 0
            scroll.scrollTo(point: CGPoint(x: 0, y: target))
            rowShift = RowShift(above: above, below: below)
        }
    }

    /// The third step: the stand-ins have their targets; the morph runs.
    private func start(_ plan: BatchMorph) {
        var still = Transaction()
        still.disablesAnimations = true
        withTransaction(still) { morph = plan }
        morphLog("start offset \(book.offset) shift \(rowShift.above),\(rowShift.below)")
        runMorph(plan)
    }

    private func morphLog(_ text: @autoclosure () -> String) {
        #if DEBUG
        let line = text()
        Self.morphLogger.debug("\(line, privacy: .public)")
        #endif
    }
    #if DEBUG
    private static let morphLogger = Logger(subsystem: "com.hachiwii.oink", category: "morph")
    #endif

    /// Plays a morph: the rows slide back in 400 ms, the stand-ins run their staggered
    /// 400 ms, then the batch's real rows show in their place.
    private func runMorph(_ plan: BatchMorph) {
        withAnimation(TransferMotion.rows) { rowShift = RowShift() }
        withAnimation(.linear(duration: plan.total / 1000)) {
            morphTime = plan.total
        } completion: {
            var still = Transaction()
            still.disablesAnimations = true
            withTransaction(still) {
                if morph?.batch == plan.batch { morph = nil }
                morphTime = 0
                collapseState.setMorphing(false)
            }
            morphLog("end offset \(book.offset)")
        }
    }

    /// Moves the list by `shift` without animation, in the same update as the
    /// layout change, so what was under the finger stays there. `maxOffset` is
    /// the largest offset of the new layout (near the list end the move is cut there).
    private func keepInPlace(_ shift: Double, maxOffset: Double? = nil) {
        guard abs(shift) > 0.5 else { return }
        var transaction = Transaction()
        transaction.disablesAnimations = true
        let target = min(max(0, book.offset + shift), maxOffset ?? .greatestFiniteMagnitude)
        // A point with x 0 (see switchLayout): never a sideways offset.
        withTransaction(transaction) { scroll.scrollTo(point: CGPoint(x: 0, y: target)) }
    }

    private var dateLabel: String {
        guard let first = ordered.first?.createdAt else { return "今天" }
        let formatter = DateFormatter()
        formatter.dateFormat = "HH:mm"
        let day = Calendar.current.isDateInToday(first) ? "今天" : Calendar.current.isDateInYesterday(first) ? "昨天"
            : DateFormatter.localizedString(from: first, dateStyle: .medium, timeStyle: .none)
        return "\(day) · \(formatter.string(from: first))"
    }

    private func isContinuation(_ entries: [TransferEntry], _ index: Int) -> Bool {
        guard index > 0 else { return false }
        let previous = entries[index - 1].last, current = entries[index].first
        if current.isMine { return previous.isMine }
        return !previous.isMine && previous.displaySenderID == current.displaySenderID && previous.proxyDeviceID == current.proxyDeviceID
    }

    struct Party: Equatable { var name: String; var platform: DevicePlatform; var online: Bool; var privileged = false }

    private func card(_ id: String?) -> IOSDeviceCard? { id.flatMap { id in source.cards?.first { $0.device == id } } }

    private func sender(of message: TransferMessage) -> Party {
        if message.isMine {
            return Party(name: source.localName ?? SystemDeviceName.current,
                         platform: UIDevice.current.userInterfaceIdiom == .pad ? .ipad : .iphone, online: true)
        }
        let card = card(message.displaySenderID)
        return Party(name: card?.name ?? message.senderName, platform: DevicePlatform(os: card?.os ?? "", name: card?.name ?? ""),
                     online: card.map { $0.connection != .offline } ?? message.sourceOnline,
                     privileged: card?.androidPrivileged == true)
    }

    private func proxy(of message: TransferMessage) -> Party? {
        guard !message.isMine, message.kind != .text, let id = message.proxyDeviceID, id != message.displaySenderID else { return nil }
        let card = card(id)
        return Party(name: card?.name ?? message.proxyName ?? id, platform: DevicePlatform(os: card?.os ?? "", name: card?.name ?? ""),
                     online: card.map { $0.connection != .offline } ?? false)
    }

    /// Download progress of a received attachment, or send progress of an own
    /// one (0–100), from the core's per-record progress.
    private func progress(of message: TransferMessage) -> Int? {
        if let id = message.recordID, let percent = source.percent[id] { return percent }
        if message.state == .stored { return 100 }
        return message.state == .downloading || message.delivery == .sending ? 0 : nil
    }

    private func download(_ message: TransferMessage) {
        Haptics.selection()
        model.download(message, open: false)
    }

    /// Long-press 复制 (`rich`, the default: a text message's rich text with
    /// its plain text) or 复制文本 (`rich: false`, the plain text only, user
    /// 2026-11-12): Oink writes the pasteboard with the record's clip marker
    /// (PasteboardCopy), then the usual copy feedback.
    private func copy(_ message: TransferMessage, rich: Bool = true) {
        let content = model.richContent(message)
        var message = message
        // 复制文本 takes the source's plain text; without one, the text of the HTML.
        if let content, (message.text ?? "").isEmpty { message.text = content.document.plainText }
        do {
            // HTML that kept no formatting is plain text: 复制 writes only the text (RichContent).
            let copied = try rich
                ? PasteboardCopy.copyRich(message, rich: content?.rich)
                : PasteboardCopy.copyPlain(message)
            guard copied else { return }
        } catch {
            model.errorMessage = error.localizedDescription
            return
        }
        Haptics.selection()
        toasts.show(AppText.pick("已复制", "Copied"))
    }

    /// Tap a file: Quick Look on it when it is here (or its thumbnail stands in), otherwise
    /// download it first and Quick Look opens it then (core.fileToOpen).
    private func open(_ message: TransferMessage) {
        if message.kind == .folder { folder = message; return }
        Haptics.selection()
        if quickLook.open(message.id, in: quickLookSource) { return }
        model.download(message, open: true)
    }

    /// Tap a picture: as a file, unless there is nothing it could show (expired, or its
    /// PC offline without a thumbnail).
    private func openImage(_ message: TransferMessage) {
        guard message.opensViewer else { return }
        open(message)
    }

    /// The conversation's sequence; a picture shown with its thumbnail downloads when reached.
    private var quickLookSource: QuickLookPresenter.Source {
        let model = model
        let original = quickLookOriginal, thumbnail = quickLookThumbnail
        return QuickLookPresenter.Source(
            entries: QuickLookSequence.entries(ordered, original: original, thumbnail: thumbnail, canPreview: Self.canPreview),
            resolve: { id in
                model.messages.first { $0.id == id }.flatMap {
                    QuickLookSequence.entry($0, original: original, thumbnail: thumbnail, canPreview: Self.canPreview)
                }
            },
            onStandInShown: { id in
                guard let message = model.messages.first(where: { $0.id == id }), message.state != .downloading else { return }
                model.download(message, open: false)
            })
    }

    static func canPreview(_ url: URL) -> Bool { QLPreviewController.canPreview(url as NSURL) }

    /// The downloaded file of a message.
    private func quickLookOriginal(_ message: TransferMessage) -> URL? {
        #if DEBUG
        if message.contentID?.hasPrefix("fixture") == true, message.kind == .image,
           (progress(of: message) ?? 100) >= 100 || message.state == .stored {
            return Self.designFile(message.id, FixturePreviewImage.image(for: message.contentID))
        }
        #endif
        return QuickLookSequence.storedFile(message)
    }

    /// The PC's thumbnail of a picture that is not downloaded.
    private func quickLookThumbnail(_ message: TransferMessage) -> URL? {
        #if DEBUG
        if message.contentID?.hasPrefix("design-thumbnail") == true {
            return Self.designFile("thumbnail-\(message.id)", FixturePreviewImage.image)
        }
        #endif
        return QuickLookSequence.thumbnailFile(message)
    }

    #if DEBUG
    /// The design states' pictures as files, for Quick Look.
    private static func designFile(_ name: String, _ image: UIImage) -> URL? {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("oink-design-\(name).png")
        if FileManager.default.fileExists(atPath: url.path) { return url }
        guard let data = image.pngData(), (try? data.write(to: url)) != nil else { return nil }
        return url
    }
    #endif

    // MARK: Composer

    private var chinese: Bool { AppText.pick("zh", "en") == "zh" }
    /// By the group's other devices (native-rewrite.md「输入框和设备选择按组内其它设备变化」,
    /// review 2026-11-11). Before the first target list the field stays usable.
    private var composerState: TransferLayout.Composer? {
        source.targets.map { TransferLayout.composer($0, chosen: source.proxyID) }
    }
    /// The device text and files go to (text is broadcast whatever it is).
    private var target: SendTarget? { composerState?.target }
    private var composerEnabled: Bool { composerState?.enabled ?? true }
    private var composerPrompt: String {
        composerState.map { TransferLayout.composerPrompt($0, chinese: chinese) } ?? (chinese ? "输入文字，随手传输…" : "Type to send…")
    }

    private var composer: some View {
        let enabled = composerEnabled
        return HStack(spacing: 10) {
            if composerState?.showsPicker ?? false { devicePicker }
            HStack(spacing: 0) {
                TextField("", text: $draft, prompt: Text(composerPrompt).font(.oink(13)).foregroundStyle(oink.hint), axis: .vertical)
                    .font(.oink(14)).foregroundStyle(oink.ink)
                    .lineLimit(1...5)
                    .focused($editing)
                    .frame(minHeight: 40)
                    .onSubmit(sendText)
                    // Nothing online to send to.
                    .disabled(!enabled)
                    .accessibilityIdentifier("transfer-composer")
                if !enabled {
                    EmptyView()
                } else if draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                    Menu {
                        Button { showCamera = true } label: { Label("拍摄", image: "Lucide-camera-2") }
                        Button { showPhotos = true } label: { Label("图库", image: "Lucide-image-2") }
                        Button { showFiles = true } label: { Label("文件", image: "Lucide-file-2") }
                    } label: {
                        Lucide(name: "plus", size: 23, color: oink.muted).frame(width: 40, height: 40)
                    }
                    .menuStyle(.button).buttonStyle(.plain).menuOrder(.fixed)
                    .accessibilityLabel("添加附件")
                } else {
                    Button(action: sendText) {
                        Lucide(name: "arrow-right", size: 22, color: oink.accentInk)
                            .frame(width: 40, height: 40)
                            .background(oink.accentSoft3, in: Circle())
                    }
                    .buttonStyle(ShrinkButtonStyle())
                    .accessibilityLabel("发送")
                }
            }
            .padding(.leading, 16).padding(.trailing, enabled ? 4 : 16).padding(.vertical, 2)
            .frame(minHeight: 48)
            // Disabled: its own darker field (--c-input-disabled); the prompt stays in hint.
            .background(enabled ? oink.fill : oink.inputDisabled, in: RoundedRectangle(cornerRadius: 26, style: .continuous))
        }
        // 8 pt above the floating tab bar (user 2026-11-10); with the keyboard up it
        // keeps sitting 6 pt above the keyboard.
        .padding(.horizontal, 16).padding(.top, 8).padding(.bottom, editing ? 6 : 8)
        .fullScreenCover(isPresented: $showCamera) {
            CameraPicker { url in showCamera = false; if let url { send(files: [url], temporary: true) } }.ignoresSafeArea()
        }
        .background(oink.bg)
        // The chosen device went offline (or left): another online one takes over,
        // computers first, with one light hint (review 2026-11-11).
        .onChange(of: TargetSwitch(chosen: source.proxyID, target: target?.id, usable: manualTargetUsable),
                  initial: true) { _, change in
            guard let chosen = change.chosen, change.usable == false else { return }
            // A choice already offline at launch was never seen online: its card names it.
            let gone = source.targetNames[chosen] ?? card(chosen)?.name
            core.setTransferProxy(nil)
            if let gone { toasts.show(TransferLayout.switchNotice(from: gone, chinese: chinese), duration: 2.5) }
        }
    }

    private var manualTargetUsable: Bool? {
        guard let targets = source.targets else { return nil }
        if targets.online.contains(where: { $0.id == source.proxyID }) { return true }
        return targets.searching ? nil : false
    }

    private struct TargetSwitch: Equatable { var chosen: String?; var target: String?; var usable: Bool? }

    private func menuItem(_ device: SendTarget) -> some View {
        Toggle(isOn: Binding(get: { source.proxyID == device.id },
                             set: { on in if on { core.setTransferProxy(device.id); toasts.hide() } })) {
            Label { Text(device.name) } icon: { MenuDeviceIcon.image(device, dark: oink.dark) }
        }
    }

    /// The device menu: online computers, then online mobile devices; a dark-grey ban
    /// sign when nothing is online (review 2026-11-11).
    private var devicePicker: some View {
        let online = source.targets?.online ?? []
        let active=online.first { $0.id==source.targets?.activeDevice } ?? TransferLayout.fallbackTarget(online)
        let computers = online.filter(\.isComputer), phones = online.filter { !$0.isComputer }
        return Menu {
            if online.isEmpty {
                Text(chinese ? "没有其它设备在线" : "No other devices online")
            } else {
                Toggle(isOn: Binding(get: { source.proxyID == nil }, set: { if $0 { core.setTransferProxy(nil) } })) {
                    // Menu extracts the first two sibling Text views as title/subtitle.
                    // A VStack inside Label loses the device subtitle in the native menu.
                    Text(chinese ? "当前活跃" : "Currently active")
                    if let active { Text(active.name); MenuDeviceIcon.image(active, dark: oink.dark) }
                }
                // Titled sections of toggles: the system draws the section titles and the
                // checkmark. (An inline Picker, alone or inside a Section, drops the titles.)
                if !computers.isEmpty {
                    Section(chinese ? "电脑" : "Computers") { ForEach(computers, content: menuItem) }
                }
                if !phones.isEmpty {
                    Section(chinese ? "移动设备" : "Mobile devices") { ForEach(phones, content: menuItem) }
                }
            }
        } label: {
            ZStack(alignment: .bottomTrailing) {
                Group {
                    if let target {
                        PlatformLogo(platform: DevicePlatform(os: target.os, name: target.name), scale: 0.8, privileged: target.privileged)
                    } else {
                        BanSign().stroke(oink.text2, style: StrokeStyle(lineWidth: 2.2, lineCap: .round)).frame(width: 25, height: 25)
                    }
                }
                .frame(width: 44, height: 48)
                DropdownCorner().fill(oink.ink).frame(width: 6, height: 6).padding(.trailing, 2).padding(.bottom, 6)
            }
            .frame(width: 44, height: 48)
        }
        .menuStyle(.button).menuOrder(.fixed)
        .buttonStyle(.plain)
        .accessibilityIdentifier("transfer-target")
        .accessibilityLabel(target.map { (chinese ? "发送到：" : "Send to: ") + $0.name } ?? (chinese ? "没有其它设备在线" : "No other devices online"))
    }

    /// Text is broadcast by the core to every online device; the bubble's
    /// delivery line (and a failure reason) comes from the record. The field
    /// is cleared only after the send succeeded; a failed send keeps the text
    /// for another try (review L17).
    private func sendText() {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, sendingText != draft else { return }
        Haptics.selection()
        let sent = draft
        sendingText = sent
        Task { @MainActor in
            do {
                _ = try await core.share(SharePayload(text: text), target: target?.id)
                draft = ComposerDraft.after(sending: sent, current: draft, succeeded: true)
            } catch { model.errorMessage = error.oinkMessage }
            if sendingText == sent { sendingText = nil }
        }
    }

    /// `scoped`: document-picker URLs, readable only inside a security scope
    /// that lasts until the core has copied the files. `temporary`: copies this
    /// page wrote (photo library, camera); the core moves them into its store,
    /// so this page removes them only when the share failed before the core
    /// took them (review L17).
    private func send(files: [URL], scoped: Bool = false, temporary: Bool = false) {
        Haptics.selection()
        Task { @MainActor in
            let open = scoped ? files.filter { $0.startAccessingSecurityScopedResource() } : []
            defer { open.forEach { $0.stopAccessingSecurityScopedResource() } }
            do { _ = try await core.share(SharePayload(text: nil, files: files, staged: temporary), target: target?.id) }
            catch {
                // Files the core already took are gone from here; the rest are ours to remove.
                if temporary { files.forEach { try? FileManager.default.removeItem(at: $0) } }
                model.errorMessage = error.oinkMessage
            }
        }
    }

    /// DEBUG design state transfer-p2p-live-offline: the UI test takes the chosen phone
    /// offline with this button once it has read the first state.
    @ViewBuilder private var designControls: some View {
        #if DEBUG
        if DesignScene.is("transfer-p2p-live-offline") {
            Button("让 iPhone 离线") { core.fixtureTakeOffline("fixture-iphone") }
                .font(.oink(10)).foregroundStyle(oink.hint)
                .padding(8)
                .accessibilityIdentifier("design-take-iphone-offline")
        }
        #endif
    }

    #if DEBUG
    /// Scene placement: the message's top (or bottom) `offset` pt below the list top (or above its bottom).
    private func place(_ id: String, _ edge: UnitPoint) {
        Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(500))
            // Twice: the first jump lands on the lazy list's estimated heights.
            scroll.scrollTo(id: id, anchor: edge)
            try? await Task.sleep(for: .milliseconds(300))
            scroll.scrollTo(id: id, anchor: edge)
        }
    }

    private func applyDesignScene(_ scene: String) {
        model.installUIFixtureIfRequested()
        switch scene {
        case "transfer-typing": draft = "晚上把会议纪要发我一下"
        case "transfer-folder", "transfer-folder-states": folder = model.messages.first { $0.kind == .folder }
        // Every download state on expanded batch items (file-bubble SPEC §4.1).
        case "transfer-file-batch-received": expanded.insert("fixture-windowsPc/fixture-op-x")
        // transfer-stack design states (prototype scenes.js): placement by message id.
        case "transfer-long": place("9010", .bottom)
        case "transfer-long-more":
            unfolded["9010"] = TransferLayout.foldedLines + TransferLayout.moreLines
            place("9010", .bottom)
        case "transfer-long-self": place("9011", .bottom)
        // A long text unfolded to 30 lines and seen, then left far above (more than half a
        // screen: the received batch expanded below it makes room): it folds again (SPEC §1.5)
        // and the list must not move (the stacks below stay where they are). Unfolded and left
        // without the batch, it stayed within half a screen of the view, where it rightly
        // stays unfolded (integration 2026-11-11).
        case "transfer-long-refold":
            unfolded["9010"] = TransferLayout.foldedLines + TransferLayout.moreLines
            expanded.insert("fixture-macMini/fixture-op-a")
            place("9010", .bottom)
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(1500))
                scroll.scrollTo(edge: .bottom)
            }
        case "transfer-stack": place("batch:fixture-macMini/fixture-op-a", .top)
        case "transfer-stack-files":
            tops["self/fixture-op-b"] = 2
            place("batch:self/fixture-op-b", .bottom)
        case "transfer-stack-swipe":
            stackDemoDrag = -50
            place("batch:fixture-macMini/fixture-op-a", .top)
        case "transfer-stack-expanded":
            expanded.insert("fixture-macMini/fixture-op-a")
            place("9020", .top)
        case "transfer-stack-expanded-self":
            expanded.insert("self/fixture-op-b")
            place("9030", .top)
        // Live expand / collapse through the real handlers (checks keeping the batch in place).
        case "transfer-stack-expand-live", "transfer-stack-collapse-live":
            let key = "fixture-macMini/fixture-op-a"
            let collapsing = scene == "transfer-stack-collapse-live"
            if collapsing { expanded.insert(key); place("9021", .top) }
            else { tops[key] = 1; place("batch:\(key)", .top) }
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(3000))
                let items = ordered.filter { TransferLayout.batchKey($0) == key }
                if collapsing { collapse(key, items: items, at: 1) } else { expand(key, items: items, top: 1) }
            }
        // Sensitive texts: both hidden; shown (the second one is being fetched).
        case "transfer-secret": place("9041", .bottom)
        case "transfer-secret-shown":
            place("9041", .bottom)
            Task { @MainActor in try? await Task.sleep(for: .milliseconds(1500)); revealed = ["9040", "9041"] }
        // Rich text (mockup scenes transfer-rich-*; the menu scenes are opened by the UI test).
        case "transfer-rich-long": place(TransferRichFixture.longID, .bottom)
        case "transfer-rich-long-more", "transfer-rich-long-more-end":
            unfolded[TransferRichFixture.longID] = TransferLayout.foldedLines + TransferLayout.moreLines
            place(TransferRichFixture.longID, scene == "transfer-rich-long-more" ? .top : .bottom)
        case "transfer-select":
            place("9010", .bottom)
            Task { @MainActor in try? await Task.sleep(for: .milliseconds(900)); selecting = "9010" }
        default: break
        }
    }

    /// The device last picked in the design states (phone-to-phone: mockup scenes.js `select`).
    private static func designChoice(_ scene: String) -> String {
        switch scene {
        case "transfer-p2p-menu-phone", "transfer-p2p-phone", "transfer-p2p-send-failed", "transfer-p2p-sending-menu",
             "transfer-p2p-cancelled-sender", "transfer-p2p-send-cancelled-by-peer", "transfer-p2p-all-offline": "fixture-android"
        case "transfer-p2p-menu-offline", "transfer-p2p-offline", "transfer-p2p-live-offline": "fixture-iphone"
        case "transfer-proxy-offline": "fixture-desktop"
        default: "fixture-windowsPc"
        }
    }
    #endif
}

/// The message long-press menu; a sensitive message gets its own preview so the
/// lifted bubble never shows the secret.
private struct MessageMenu<Items: View, Preview: View>: ViewModifier {
    let sensitive: Bool
    @ViewBuilder let items: () -> Items
    @ViewBuilder let preview: () -> Preview
    func body(content: Content) -> some View {
        if sensitive {
            content.contextMenu(menuItems: items, preview: preview)
        } else {
            content.contextMenu(menuItems: items)
        }
    }
}

/// What the transfer page shows from CoreSession, published only when that changes.
/// The page used to observe the whole session, and the session publishes about once a
/// second while computers are connected (latency of the device cards) besides status,
/// nearby, audio and other changes the page never shows; each one rebuilt the page in
/// the middle of a scroll (20–35 ms on the simulator with a 400-record conversation).
@MainActor
final class TransferPageSource: ObservableObject {
    /// Group members without the fields the page does not show (latency, last seen, controls).
    @Published private(set) var cards: [IOSDeviceCard]?
    /// Whole-percent progress by record (the page shows nothing finer).
    @Published private(set) var percent: [Int64: Int] = [:]
    @Published private(set) var proxyID: String?
    @Published private(set) var localName: String?
    @Published private(set) var fileToOpen: IOSFileToOpen?
    /// Who can be sent to (the composer's devices).
    @Published private(set) var targets: SendTargets?
    /// Names of the devices seen as targets, so「X 已离线」can name one that left the list.
    private(set) var targetNames: [String: String] = [:]

    init(core: CoreSession = .shared) {
        core.$cards.map { $0?.cards.map(Self.shown) }.removeDuplicates().assign(to: &$cards)
        core.progress.$records.map { $0.compactMapValues { $0.fraction.map { Int(($0 * 100).rounded()) } } }
            .removeDuplicates().assign(to: &$percent)
        core.$transferProxyID.removeDuplicates().assign(to: &$proxyID)
        core.$localDeviceName.map { $0?.name }.removeDuplicates().assign(to: &$localName)
        core.$fileToOpen.removeDuplicates().assign(to: &$fileToOpen)
        core.$transferTargets.removeDuplicates().map { [weak self] targets in
            for device in (targets?.online ?? []) + [targets?.onlyDevice].compactMap({ $0 }) { self?.targetNames[device.id] = device.name }
            return targets
        }.assign(to: &$targets)
    }

    private static func shown(_ card: IOSDeviceCard) -> IOSDeviceCard {
        var card = card
        card.latencyMs = nil
        card.lastSeenMs = nil
        card.controls = DeviceControls()
        #if OINK_MOBILE_CORE_FFI
        card.source = nil
        #endif
        return card
    }
}

/// What only 收起 depends on: whether the list scrolls, and which items of expanded
/// batches are completely on screen. It changes as the list scrolls and rows pass the
/// edges, so it is not TransferView state: that would rebuild the whole page (every
/// visible row) at each scroll start and stop and each row passing an edge. Only the
/// 收起 chips observe it.
@MainActor
final class CollapseState: ObservableObject {
    @Published private(set) var scrolling = false
    /// A batch is expanding or collapsing (收起 waits for it, as for a scroll).
    @Published private(set) var morphing = false
    /// Fully visible items of expanded batches, with their row index.
    @Published private(set) var fullyVisible: [String: Int] = [:]

    func setScrolling(_ value: Bool) {
        if scrolling != value { scrolling = value }
    }

    func setMorphing(_ value: Bool) {
        if morphing != value { morphing = value }
    }

    /// `index` nil: not (or no longer) completely on screen.
    func setFullyVisible(_ id: String, at index: Int?) {
        if fullyVisible[id] != index { fullyVisible[id] = index }
    }

    /// The item carrying 收起: the first fully visible item of the topmost
    /// expanded batch on screen (SPEC §3.1), shown while the list rests.
    func shows(_ id: String) -> Bool {
        !scrolling && !morphing && fullyVisible.min { $0.value < $1.value }?.key == id
    }
}

/// The geometry a message row reports. The observers of the row's place in the list
/// run on every scrolled frame, so they measure only for the rows that use it.
private struct RowTracking: ViewModifier {
    let tracking: MessageRow.Tracking?
    let selecting: Bool
    let opened: Bool
    let inBatch: Bool

    func body(content: Content) -> some View {
        if let tracking {
            content
                .onGeometryChange(for: CGRect.self) { selecting ? $0.frame(in: .named(TransferView.listSpace)) : .zero } action: {
                    tracking.selectionFrame($0)
                }
                .onChange(of: selecting) { _, value in if value { tracking.selectionFrame(.zero) } }
                .onGeometryChange(for: Int.self) { opened ? TransferView.side($0) : 0 } action: { tracking.side($0) }
                // In the content's coordinates: independent of the scroll offset, which the list
                // may report after the row's geometry.
                .onGeometryChange(for: CGRect.self) { opened ? $0.frame(in: .named(TransferView.contentSpace)) : .null } action: {
                    tracking.place($0)
                }
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { tracking.height($0) }
                .onDisappear { tracking.gone() }
                .onGeometryChange(for: Bool.self) { inBatch && TransferView.fullyInView($0) } action: { tracking.fullyVisible($0) }
        } else {
            content
        }
    }
}

/// 收起 of one expanded batch item; the only view that observes CollapseState.
private struct CollapseChip: View {
    let id: String
    let mine: Bool
    @ObservedObject var state: CollapseState
    let action: () -> Void

    var body: some View {
        let shown = state.shows(id)
        BatchChip(icon: "chevrons-down-up", title: AppText.pick("收起", "Hide"),
                  accessibility: AppText.pick("收起为一叠", "Stack again"), action: action)
            .fixedSize()
            .frame(width: 0, alignment: mine ? .trailing : .leading)
            .offset(x: mine ? -12 : 12)
            .opacity(shown ? 1 : 0)
            .scaleEffect(shown ? 1 : 0.9)
            .allowsHitTesting(shown)
            .animation(shown ? .timingCurve(0.2, 0.9, 0.3, 1.2, duration: 0.15) : .easeIn(duration: 0.12), value: shown)
    }
}

/// Mutable box for values the list reports every frame that no view renders.
final class ScrollBook {
    var offset: CGFloat = 0
    /// Largest content offset of the current layout.
    var maxOffset: CGFloat = 0
    /// Content and viewport heights.
    var content: CGFloat = 0
    var viewport: CGFloat = 0
    /// Frames in the list's viewport (TransferView.listSpace) of the bubbles of expanded
    /// batch items (by message id) and of the stack boxes (boxKey), while laid out.
    var frames: [String: CGRect] = [:]
    static func boxKey(_ batch: String) -> String { "box:\(batch)" }
    static func rowKey(_ entry: String) -> String { "row:\(entry)" }
    /// Last height and side (TransferView.side) of each row, and folds waiting for compensation.
    var heights: [String: CGFloat] = [:]
    var sides: [String: Int] = [:]
    var pending: [String: (height: CGFloat, offset: CGFloat)] = [:]
    /// Opened rows (unfolded, shown or selected): their last place in the content
    /// (top at offset 0) and height, kept when the lazy list drops them.
    var opened: [String: (top: CGFloat, height: CGFloat)] = [:]
}

/// Menu item images of the devices: the platform logo in its own colours (Windows blue,
/// Android green; iPhone / iPad keep their corner mark), drawn once per platform and scheme
/// and kept out of the menu's tint.
@MainActor
private enum MenuDeviceIcon {
    private struct Key: Hashable { var platform: DevicePlatform; var dark: Bool }
    private static var made: [Key: UIImage] = [:]
    static func image(_ device: SendTarget, dark: Bool) -> Image {
        let key = Key(platform: DevicePlatform(os: device.os, name: device.name), dark: dark)
        if let hit = made[key] { return Image(uiImage: hit) }
        let renderer = ImageRenderer(content: PlatformLogo(platform: key.platform, scale: 0.72)
            .frame(width: 24, height: 24).environment(\.oink, OinkPalette(dark: dark, accent: .pink)))
        renderer.scale = UIScreen.main.scale
        let image = (renderer.uiImage ?? UIImage()).withRenderingMode(.alwaysOriginal)
        made[key] = image
        return Image(uiImage: image)
    }
}

/// Lucide `ban`: a circle with a slash (no device online to send to).
private struct BanSign: Shape {
    func path(in rect: CGRect) -> Path {
        let s = min(rect.width, rect.height) / 24
        var path = Path(ellipseIn: CGRect(x: 2 * s, y: 2 * s, width: 20 * s, height: 20 * s))
        path.move(to: CGPoint(x: 4.93 * s, y: 4.93 * s))
        path.addLine(to: CGPoint(x: 19.07 * s, y: 19.07 * s))
        return path.offsetBy(dx: rect.minX, dy: rect.minY)
    }
}

/// The small triangle at the proxy button's corner (`.tr-dropdown-corner`).
private struct DropdownCorner: Shape {
    func path(in rect: CGRect) -> Path {
        var path = Path()
        path.move(to: CGPoint(x: rect.maxX, y: rect.minY))
        path.addLine(to: CGPoint(x: rect.maxX, y: rect.maxY))
        path.addLine(to: CGPoint(x: rect.minX, y: rect.maxY))
        path.closeSubpath()
        return path
    }
}

// MARK: - Batch morph

/// An expand or collapse in progress (Android BatchMorph): stand-ins for the batch's items.
struct BatchMorph {
    let batch: String
    let expanding: Bool
    /// The top card (expand) or the item 收起 was tapped on (collapse).
    let anchor: Int
    let mine: Bool
    var ghosts: [MorphGhostPlan]
    /// The whole morph: 400 ms plus the last item's stagger.
    var total: Double { TransferLayout.morphMs + (ghosts.map(\.delay).max() ?? 0) }
}

/// One item's stand-in: where it starts and ends, and which looks it shows on the way.
struct MorphGhostPlan: Identifiable {
    var id: String { message.id }
    let message: TransferMessage
    let picture: ListPicture?
    let progress: Int?
    let detail: String
    let delay: Double
    var hasCard = false
    var hasBubble = false
    var toBubble = false
    var from: TransferLayout.MorphGeo?
    var to: TransferLayout.MorphGeo?
    /// The message's size (lays out a file's message face).
    var bubble: CGSize?
}

/// How far the rows around a morphing batch are drawn from their new places: the rows
/// above it and the batch's first row (its sender line), and the rows below it.
struct RowShift: Equatable {
    var above: CGFloat = 0
    var below: CGFloat = 0

    /// `batch`: the batch's row indices in the new layout (empty: no morph).
    func offset(index: Int, batch: [Int]) -> CGFloat {
        guard let first = batch.first else { return 0 }
        if index <= first { return above }
        return batch.contains(index) ? 0 : below
    }
}

/// What the list reports of its scroll geometry.
struct ListGeometry: Equatable {
    var offset: CGFloat
    var content: CGFloat
    var viewport: CGFloat
}

/// Reports a view's frame in the list's viewport while it is laid out (nil once it is not).
private struct FrameReport: ViewModifier {
    let report: ((CGRect?) -> Void)?
    func body(content: Content) -> some View {
        if let report {
            content
                .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(TransferView.listSpace)) } action: { report($0) }
                .onDisappear { report(nil) }
        } else {
            content
        }
    }
}

/// One item's stand-in, drawn on every frame of the morph (`time` in ms animates): it
/// moves and resizes from card to message (or back) with standard easing after its
/// stagger, and a file's card content cross-fades with its message content. A file is a
/// card at both ends (native-rewrite.md「文件卡片」): its plate, radius and 1 pt edge stay,
/// only the stack's shadow fades; a picture's corners go to the message's.
private struct MorphGhost: View, Animatable {
    @Environment(\.oink) private var oink
    var time: Double
    let ghost: MorphGhostPlan
    let mine: Bool

    var animatableData: Double {
        get { time }
        set { time = newValue }
    }

    var body: some View {
        if let from = ghost.from {
            let to = ghost.to ?? from
            let e = TransferLayout.easeStandard((time - ghost.delay) / TransferLayout.morphMs)
            let lerp = { (a: Double, b: Double) in TransferLayout.lerp(a, b, e) }
            let media = ghost.message.kind == .image
            let colors = StackColors(dark: oink.dark)
            // Corners: top leading, top trailing, bottom trailing, bottom leading.
            let radii = { (geo: TransferLayout.MorphGeo) -> [Double] in
                !media ? Array(repeating: 12, count: 4) : geo.card ? Array(repeating: 9, count: 4) : (mine ? [18, 4, 18, 18] : [4, 18, 18, 18])
            }
            let a = radii(from), b = radii(to)
            let shape = UnevenRoundedRectangle(topLeadingRadius: lerp(a[0], b[0]), bottomLeadingRadius: lerp(a[3], b[3]),
                                               bottomTrailingRadius: lerp(a[2], b[2]), topTrailingRadius: lerp(a[1], b[1]),
                                               style: .continuous)
            let rect = CGRect(x: lerp(from.rect.minX, to.rect.minX), y: lerp(from.rect.minY, to.rect.minY),
                              width: lerp(from.rect.width, to.rect.width), height: lerp(from.rect.height, to.rect.height))
            let cardLook = lerp(from.card ? 1 : 0, to.card ? 1 : 0)
            let faces = TransferLayout.morphFaces(e: e, hasCard: ghost.hasCard, hasBubble: ghost.hasBubble, toBubble: ghost.toBubble)
            ZStack(alignment: .topLeading) {
                media ? Color.clear : colors.card
                if media {
                    StackPicture(message: ghost.message, picture: ghost.picture, progress: ghost.progress)
                } else {
                    // The card's content stays in the top-left corner and fades; the message's
                    // content fades in at the message's size (or the reverse).
                    if faces.card > 0 {
                        StackFileContent(message: ghost.message, detail: ghost.detail).opacity(faces.card)
                    }
                    if faces.bubble > 0, let size = ghost.bubble {
                        FileCardContent(message: ghost.message, progress: ghost.progress, fixedWidth: 220)
                            .frame(width: size.width, height: size.height, alignment: .topLeading)
                            .opacity(faces.bubble)
                    }
                }
            }
            .frame(width: rect.width, height: rect.height, alignment: .topLeading)
            .clipShape(shape)
            // A picture's edge (none on its placeholder), or the file card's.
            .overlay(shape.strokeBorder(media ? (ghost.picture == nil ? Color.clear : colors.mediaEdge) : colors.edge,
                                        lineWidth: media ? 0.5 : StackColors.edgeWidth))
            .colorMultiply(Color(white: 1 - min(1, colors.dim * lerp(from.depth, to.depth))))
            .compositingGroup()
            .stackShadow(colors, strength: cardLook)
            .scaleEffect(lerp(from.scale, to.scale))
            .rotationEffect(.degrees(lerp(from.rotation, to.rotation)))
            .offset(x: rect.minX + lerp(from.dx, to.dx), y: rect.minY)
            .opacity(lerp(from.opacity, to.opacity))
        }
    }
}

// MARK: - Message

/// Equatable on what it shows, not its callbacks: the page rebuilds on every
/// CoreSession change (device cards, transfer events) and when the scroll position
/// moves to another row, and without this every visible row was rebuilt and laid out
/// again each time. The callbacks read the page's state when they run, and every value
/// they capture is compared here, so a row kept with its old callbacks acts the same.
private struct MessageRow: View, Equatable {
    @Environment(\.oink) private var oink
    /// Collapsed batch: its items, the top card and the callbacks of the stack.
    struct Stack {
        var items: [TransferMessage]
        var top: Int
        var demoDrag: CGFloat?
        /// Progress of each item (TransferView.progress(of:)).
        var progress: [Int?]
        /// 展开 fades in once shown (after the batch collapsed).
        var enter = false
        var onMove: (Int) -> Void
        var onExpand: () -> Void
        var onEntered: () -> Void = {}
        /// The 120 × 160 box's frame in the list's viewport (nil: no longer laid out).
        var onBoxFrame: (CGRect?) -> Void = { _ in }
    }

    static func == (a: MessageRow, b: MessageRow) -> Bool {
        a.message == b.message && a.continuation == b.continuation && a.sender == b.sender && a.proxy == b.proxy
            && a.progress == b.progress && a.bodyWidth == b.bodyWidth && a.hidden == b.hidden
            && a.inBatch == b.inBatch && a.lines == b.lines
            && a.selecting == b.selecting && a.revealed == b.revealed && (a.onCollapse == nil) == (b.onCollapse == nil)
            && a.collapseState === b.collapseState && a.reduceMotion == b.reduceMotion
            && a.index == b.index && a.opened == b.opened && a.peerName == b.peerName && (a.tracking == nil) == (b.tracking == nil)
            && a.stack?.items == b.stack?.items && a.stack?.top == b.stack?.top && a.stack?.demoDrag == b.stack?.demoDrag
            && a.stack?.progress == b.stack?.progress && a.stack?.enter == b.stack?.enter
            && (a.onBubbleFrame == nil) == (b.onBubbleFrame == nil) && a.rich === b.rich
    }
    /// The record shown; for a stack its top card (with the batch's send state).
    let message: TransferMessage
    let continuation: Bool
    let sender: TransferView.Party
    let proxy: TransferView.Party?
    let progress: Int?
    /// The page's Reduce Motion, which its callbacks animate with (compared in ==).
    let reduceMotion: Bool
    /// The widest the avatar's neighbour may get (TransferLayout.bodyMaxWidth).
    var bodyWidth: CGFloat = .infinity
    /// The batch is expanding or collapsing: stand-ins over the list draw the bubble and
    /// its status; the sender line stays.
    var hidden = false
    var stack: Stack? = nil
    /// An item of an expanded batch (normal message look of SPEC §3).
    var inBatch = false
    /// An item of an expanded batch: its bubble's frame in the list's viewport, for the morph.
    var onBubbleFrame: ((CGRect?) -> Void)? = nil
    /// Text: lines shown before 查看更多.
    var lines = TransferLayout.foldedLines
    var onMore: () -> Void = {}
    /// Text: in system text selection (长按「选择」).
    var selecting = false
    var onSelect: () -> Void = {}
    var onEndSelect: () -> Void = {}
    /// Sensitive text: shown (显示) or `***`.
    var revealed = false
    var onReveal: (Bool) -> Void = { _ in }
    /// An item of an expanded batch: 收起 (shown on the first fully visible item, not while scrolling).
    var onCollapse: (() -> Void)? = nil
    var collapseState: CollapseState? = nil
    let onOpenImage: () -> Void
    /// The image bubble became visible: lazily ask for its thumbnail.
    var onImageVisible: () -> Void = {}
    let onOpenFile: () -> Void
    let onDownload: () -> Void
    let onCopy: () -> Void
    /// 复制文本: a rich text's plain text only.
    var onCopyText: () -> Void = {}
    let onDelete: () -> Void
    /// The other phone of a phone-to-phone record (「已发送给 X」).
    var peerName: String? = nil
    /// Long-press「取消发送」/「取消接收」.
    var onCancelTransfer: () -> Void = {}
    /// A text's rich text (TransferModel.richContent): drawn formatted, with 复制文本.
    var rich: RichContent? = nil
    /// The row's place in the conversation (orders the fully visible batch items for 收起).
    var index = 0
    /// Unfolded, shown (secret) or selected: the row follows its distance from the view.
    var opened = false
    /// Where a message row is in the list (stack rows have none).
    var tracking: Tracking? = nil
    @State private var textLines: Int?
    /// A rich text's laid-out height (it folds by height, RichLayout.folds).
    @State private var richHeight: CGFloat?

    /// Callbacks of a message row's geometry. Inside the row, so an unchanged row
    /// (==) costs nothing when the page rebuilds, however many rows the lazy list keeps.
    struct Tracking {
        var selectionFrame: (CGRect) -> Void
        var side: (Int) -> Void
        /// Where an opened row is, in the list's content (null once it is not opened).
        var place: (CGRect) -> Void
        var height: (CGFloat) -> Void
        var gone: () -> Void
        var fullyVisible: (Bool) -> Void
    }
    private var mine: Bool { message.isMine }

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            if !mine { avatar }
            VStack(alignment: mine ? .trailing : .leading, spacing: 0) {
                if !continuation { meta.padding(.bottom, 6) }
                Group {
                    content
                        // 收起 sits outside the item, on the open side (away from the avatar).
                        .overlay(alignment: mine ? .leading : .trailing) { collapseChip }
                    // Successful captions stay on every direct phone bubble; other sends have no success line.
                    if mine && (message.delivery != .sent || message.isDirect) && TransferLayout.showsDeliveryLine(state: message.delivery, direct: message.isDirect) { delivery }
                    if !mine && message.isDirect && (stack == nil || message.state == .stored) { receiveStatus }
                }
                .opacity(hidden ? 0 : 1)
            }
            // The body takes the row beside the avatar (the mockup's 100% − 44), capped on
            // wide screens; no Spacer beside it, which would share the row with it.
            .frame(maxWidth: bodyWidth, alignment: mine ? .trailing : .leading)
            .frame(maxWidth: .infinity, alignment: mine ? .trailing : .leading)
            if mine { avatar }
        }
        .modifier(RowTracking(tracking: tracking, selecting: selecting, opened: opened, inBatch: inBatch))
    }

    @ViewBuilder private var menuItems: some View {
        // Copy first, as in the system menus; files, folders and a collapsed stack have none
        // (user 2026-11-12). While the text is being selected the system selection owns the long press.
        if let flight = TransferLayout.inFlight(message) {
            // A transfer in flight offers only stopping it (review 2026-11-11).
            switch flight {
            case .sending:
                Button(AppText.pick("取消发送", "Cancel sending"), role: .destructive, action: onCancelTransfer)
            case .receiving:
                Button(AppText.pick("取消接收", "Cancel receiving"), action: onCancelTransfer)
                Button(AppText.pick("删除", "Delete"), role: .destructive, action: onDelete)
            }
        } else if !selecting {
            ForEach(TransferLayout.menu(message, rich: rich != nil && stack == nil, collapsedStack: stack != nil,
                                        revealed: revealed), id: \.self) { item in
                switch item {
                case .copy: Button(AppText.pick("复制", "Copy"), action: onCopy)
                case .copyText: Button(AppText.pick("复制文本", "Copy text"), action: onCopyText)
                case .select: Button(AppText.pick("选择", "Select"), action: onSelect)
                case .delete: Button("删除", role: .destructive, action: onDelete)
                }
            }
        }
    }

    /// Long-press preview of a sensitive message: the hidden bubble, never the text.
    private var concealedPreview: some View {
        Text("***").font(LongTextBody.font).foregroundStyle(oink.hint)
            .padding(.horizontal, 14).padding(.vertical, 13.5)
            .background(mine ? oink.accentBubble : oink.fill)
    }

    /// 收起 (SPEC §3.1): 12 pt outside the item, vertically centred; it hides
    /// (120 ms) while the list scrolls and comes back (150 ms) 80 ms after it rests.
    @ViewBuilder private var collapseChip: some View {
        if let onCollapse, let collapseState {
            CollapseChip(id: message.id, mine: mine, state: collapseState, action: onCollapse)
        }
    }

    private var avatar: some View {
        Group {
            if continuation { Color.clear } else { PlatformLogo(platform: sender.platform, scale: 0.85, privileged: sender.privileged) }
        }
        .frame(width: 32, height: 30.6, alignment: .top)
        .padding(.top, 20)
        .accessibilityHidden(true)
    }

    private var meta: some View {
        HStack(spacing: 6) {
            Text(sender.name).lineLimit(1).truncationMode(.tail).frame(maxWidth: 220, alignment: mine ? .trailing : .leading).fixedSize()
            if let proxy {
                HStack(spacing: 3) {
                    Text("经")
                    PlatformLogo(platform: proxy.platform, scale: 11.0 / 28, color: proxy.platform == .mac ? oink.muted : nil)
                        .frame(width: 12, height: 12)
                    Text(proxy.name).lineLimit(1).fixedSize()
                    dot(proxy.online).padding(.leading, 3)
                }
                .foregroundStyle(oink.hint)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel("经 \(proxy.name) 转发，\(proxy.name)\(proxy.online ? "在线" : "离线")")
            } else {
                dot(sender.online).accessibilityLabel(sender.online ? "在线" : "离线")
            }
        }
        .font(.oink(10))
        .foregroundStyle(oink.muted)
        .frame(height: 16)
    }

    private func dot(_ online: Bool) -> some View {
        Circle().fill(online ? oink.good : oink.dotOff).frame(width: 5, height: 5)
    }

    private var bubbleShape: UnevenRoundedRectangle {
        UnevenRoundedRectangle(topLeadingRadius: mine ? 18 : 4, bottomLeadingRadius: 18, bottomTrailingRadius: 18, topTrailingRadius: mine ? 4 : 18, style: .continuous)
    }

    @ViewBuilder private var content: some View {
        if let stack {
            stackView(stack).onAppear(perform: onImageVisible)
        } else {
            switch message.kind {
            case .text:
                textMessage
            case .image:
                withMenu(Button(action: onOpenImage) { inBatch ? AnyView(batchImage) : AnyView(imageView) }
                    .buttonStyle(.plain)
                    .background(QuickLookAnchor(id: message.id)), shape: bubbleShape)
                    .accessibilityLabel(imageAccessibility)
                    .accessibilityIdentifier("transfer-image-\(message.id)")
                    .onAppear(perform: onImageVisible)
            case .file, .richText, .folder:
                // The long-press preview takes the card's own rounded rectangle.
                withMenu(fileCard.background(QuickLookAnchor(id: message.id)),
                         shape: RoundedRectangle(cornerRadius: 12, style: .continuous))
                    .modifier(FrameReport(report: onBubbleFrame))
            }
        }
    }

    /// The long-press menu on the bubble itself (a stack: its cards), never on the row:
    /// on the full-width row the system lifted and highlighted the whole row and placed
    /// the menu under the row's centre, away from the bubble (user, iPhone Air). The
    /// preview takes the bubble's own outline.
    private func withMenu<V: View, S: Shape>(_ view: V, shape: S) -> some View {
        view
            .contentShape(.contextMenuPreview, shape)
            .modifier(MessageMenu(sensitive: message.sensitive, items: { menuItems }, preview: { concealedPreview }))
    }

    /// A text bubble; a shown secret has 隐藏 beside it, on the open side like 展开 beside a
    /// stack (user 2026-11-11), which covers it again.
    @ViewBuilder private var textMessage: some View {
        if message.sensitive && revealed && message.text != nil {
            let chip = BatchChip(icon: "chevrons-down-up", title: AppText.pick("隐藏", "Hide"),
                                 accessibility: AppText.pick("隐藏密码", "Hide password")) { onReveal(false) }
            HStack(spacing: 10) {
                if mine { chip; withMenu(textBubble, shape: bubbleShape) } else { withMenu(textBubble, shape: bubbleShape); chip }
            }
        } else {
            withMenu(textBubble, shape: bubbleShape)
        }
    }

    /// Long text folds to `lines` lines with the last three fading; 查看更多 is a
    /// centred row at the bubble bottom (SPEC §1). Copy always takes the whole text.
    /// A hidden secret shows *** with a 显示 row; while its body is on the way neither
    /// 显示 nor 隐藏 shows (user 2026-11-11).
    private var textBubble: some View {
        let hidden = message.sensitive && !revealed
        // A secret whose body is on the way: shown, or already being fetched.
        let fetching = message.sensitive && message.text == nil && (revealed || message.state == .downloading)
        let showRow = hidden && !fetching
        let richBody = hidden || fetching ? nil : rich
        let folded = !hidden && !fetching
            && (richBody != nil ? RichLayout.folds(height: richHeight ?? 0, shown: lines) : (textLines ?? 0) > lines)
        // A text that wraps takes the whole width the message may use, as on Android and
        // in the mockup (a wrapped CSS / Compose text block fills its line width). A SwiftUI
        // Text hugs its longest line instead, and CJK lines break short of the edge, so long
        // own bubbles ended up to 20 pt away from the left margin and received ones from the
        // right: the conversation looked shifted and its margins uneven (user, iPhone Air).
        // A rich body sizes itself (RichTextBody: hugs, or the whole width once it wraps).
        let wraps = richBody == nil && !hidden && !fetching && (textLines ?? 0) > 1
        // Rich lines carry their own leading (23.1 a line, as CSS); the padding takes it off
        // so a one-line rich text is as tall as a plain one.
        let vertical = richBody == nil ? 13.5 : 13.5 - RichFonts.bodyHalfLeading
        return VStack(alignment: .leading, spacing: 0) {
            if hidden {
                Text("***").font(LongTextBody.font).foregroundStyle(oink.hint)
                    .accessibilityLabel(AppText.pick("已隐藏的密码", "Hidden password"))
            } else if fetching {
                HStack(spacing: 8) {
                    ProgressRing(value: progress ?? 0, size: 22)
                    Text(AppText.pick("正在获取…", "Fetching…")).font(LongTextBody.font).foregroundStyle(oink.hint)
                }
            } else if let richBody {
                RichTextBody(document: richBody.document, shown: lines, height: $richHeight,
                             selecting: selecting, onEndSelect: onEndSelect)
                    // A container with its own identifier: on the bare blocks the identifier went
                    // to every element inside and replaced theirs (the tables' transfer-rich-table,
                    // integration 400006).
                    .accessibilityElement(children: .contain)
                    .accessibilityIdentifier("transfer-rich-\(message.id)")
            } else {
                // A received text whose body is not here yet says why, never an empty bubble.
                LongTextBody(text: message.text ?? (mine ? "" : message.pendingTextStatus), shown: lines,
                             color: message.text == nil ? oink.hint : oink.bubbleInk, lines: $textLines,
                             selecting: selecting, onEndSelect: onEndSelect)
            }
            // Room for the rows below; they are laid over it at the text's width,
            // so a short secret keeps a compact bubble.
            Color.clear.frame(width: 0, height: CGFloat((folded ? 1 : 0) + (showRow ? 1 : 0)) * 36)
        }
        .frame(maxWidth: wraps ? .infinity : nil, alignment: .leading)
        // A hidden secret keeps at least the width of its 显示 row.
        .frame(minWidth: showRow ? 92 : nil, alignment: .leading)
        .overlay(alignment: .bottom) {
            VStack(spacing: 0) {
                if folded { MoreRow(action: onMore) }
                if showRow { MoreRow(title: AppText.pick("显示", "Show")) { onReveal(true) } }
            }
            .padding(.horizontal, -14)
        }
        .padding(.horizontal, 14).padding(.top, vertical).padding(.bottom, folded || showRow ? 4 : vertical)
        .background(mine ? oink.accentBubble : oink.fill, in: bubbleShape)
        .modifier(RetryTap(enabled: message.textRetryable, action: onDownload))
    }

    /// 220 wide on both sides at the original's proportions (0.75–2), 260 : 146 until they
    /// are known (native-rewrite.md「图片尺寸」, user 2026-11-11). An own picture has its
    /// original's size from the send (it used to be cropped to 260 : 146).
    private var imageView: some View {
        let size = TransferLayout.imageSize(aspect: TransferLayout.imageAspect(message.imageAspect))
        return ZStack {
            if undelivered {
                oink.fill
                imageStatus
            } else if let original = Self.original(message, progress: progress) {
                ListPictureView(original) { oink.fill }
            } else if let thumbnail = Self.thumbnail(message) {
                ListPictureView(thumbnail) { oink.fill }
                if message.state == .downloading {
                    Color.black.opacity(0.28)
                    ProgressRing(value: progress ?? 0, trackColor: .white.opacity(0.3))
                }
            } else {
                oink.fill
                imageStatus
            }
        }
        .frame(width: size.width, height: size.height)
        .clipShape(bubbleShape)
        .mediaEdge(bubbleShape, shown: showsPicture)
        // A filled picture overflows the bubble; clipping hides it but does not stop it
        // from taking taps, so a portrait screenshot covered the messages around it.
        .contentShape(bubbleShape)
    }

    /// The bubble shows the picture (original or thumbnail), not a placeholder: it gets the edge.
    private var showsPicture: Bool {
        !undelivered && (Self.original(message, progress: progress) ?? Self.thumbnail(message)) != nil
    }

    /// A direct (phone-to-phone) image that failed or was cancelled before it arrived:
    /// the placeholder says so instead of any partial picture or thumbnail.
    private var undelivered: Bool {
        !mine && message.isDirect && (message.state == .failed || message.state == .cancelled)
    }

    /// The placeholder's content for each state without a picture.
    @ViewBuilder private var imageStatus: some View {
        if !mine && message.isDirect && message.state == .failed {
            ImageNotice(icon: "alert-circle", text: AppText.pick("接收失败", "Not received"))
        } else if !mine && message.isDirect && message.state == .cancelled {
            ImageNotice(icon: "x", text: AppText.pick("已取消", "Cancelled"))
        } else {
            previewStatus
        }
    }

    @ViewBuilder private var previewStatus: some View {
        switch message.imagePreview {
        case .expired:
            ImageNotice(icon: "image-off", text: AppText.pick("图片已失效", "Image expired"))
        case .offline:
            ImageNotice(icon: "wifi-off", text: AppText.pick("暂时无法加载", "Unavailable for now"))
        case .failed where message.state != .downloading:
            ImageNotice(icon: "refresh-cw", text: AppText.pick("加载失败，点按重试", "Failed, tap to retry"))
        case .loading where message.state != .downloading:
            OinkSpinner(size: 22)
        default:
            if !mine, message.state == .downloading || message.imagePreview == nil { ProgressRing(value: progress ?? 0) }
        }
    }

    private var imageAccessibility: String {
        if !mine && message.isDirect && message.state == .failed { return AppText.pick("图片接收失败", "Image not received") }
        if !mine && message.isDirect && message.state == .cancelled { return AppText.pick("图片已取消", "Image cancelled") }
        return switch message.imagePreview {
        case .expired: AppText.pick("图片已失效", "Image expired")
        case .offline: AppText.pick("图片暂时无法加载", "Image unavailable for now")
        default: AppText.pick("打开图片", "Open image")
        }
    }

    /// Batch image (SPEC §3): contained in 220 × 280, bubble corners, 0.5 edge, no shadow.
    private var batchImage: some View {
        let picture = Self.picture(message, progress: progress)
        let size = TransferLayout.fit(aspect: picture?.aspect ?? 3.0 / 4, in: TransferLayout.expandedImageBox)
        return ZStack {
            if let picture, !undelivered {
                ListPictureView(picture) { oink.fill }
            } else {
                oink.fill
                imageStatus
            }
        }
        .frame(width: size.width, height: size.height)
        .clipShape(bubbleShape)
        .contentShape(bubbleShape)
        .mediaEdge(bubbleShape, shown: picture != nil && !undelivered)
        .modifier(FrameReport(report: onBubbleFrame))
    }

    /// The downloaded image. Pictures are decoded at the list size off the main
    /// thread (ImageThumbnails), never in a row's body.
    static func original(_ message: TransferMessage, progress: Int?) -> ListPicture? {
        #if DEBUG
        if message.contentID?.hasPrefix("fixture") == true, (progress ?? 100) >= 100 || message.state == .stored {
            return .fixture(FixturePreviewImage.image(for: message.contentID))
        }
        #endif
        guard message.state == .stored, let path = message.localPath else { return nil }
        return .file(path)
    }

    /// The PC's preview of an image that is not downloaded (protocol-v2 §4.4).
    static func thumbnail(_ message: TransferMessage) -> ListPicture? {
        #if DEBUG
        if message.contentID?.hasPrefix("design-thumbnail") == true { return .fixture(FixturePreviewImage.image) }
        #endif
        guard message.imagePreview == .thumbnail, let path = message.thumbnailPath else { return nil }
        return .file(path)
    }

    /// The downloaded image, else the PC's thumbnail (batch items, stack cards, layout).
    static func picture(_ message: TransferMessage, progress: Int?) -> ListPicture? {
        original(message, progress: progress) ?? thumbnail(message)
    }

    // MARK: Stack

    /// The collapsed batch with 展开 N on its open side (SPEC §2.2–2.3); a tap acts
    /// on the top card (image viewer, open file, folder page).
    private func stackView(_ stack: Stack) -> some View {
        let ids = stack.items.map(\.id)
        let box = BatchStack(ids: ids, top: stack.top, demoDrag: stack.demoDrag, onMove: stack.onMove) { index in
            let item = stack.items[index]
            StackCardFace(message: item, picture: Self.picture(item, progress: stack.progress[index]),
                          progress: stack.progress[index], detail: Self.stackDetail(item))
        }
        .modifier(FrameReport(report: stack.onBoxFrame))
        // Quick Look zooms out of (and back into) the top card.
        .background(QuickLookAnchor(id: message.id))
        .padding(.vertical, 4)
        // Room for the peeking cards: 12 on each side, 11 toward the avatar.
        .padding(.leading, mine ? 12 : 11).padding(.trailing, mine ? 11 : 12)
        .onTapGesture { message.kind == .image ? onOpenImage() : onOpenFile() }
        // Long press: the top card's menu, lifted with the cards (not the 展开 chip).
        .contentShape(.contextMenuPreview, RoundedRectangle(cornerRadius: 12, style: .continuous))
        .modifier(MessageMenu(sensitive: message.sensitive, items: { menuItems }, preview: { concealedPreview }))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(stack.items.count) 项，当前第 \(stack.top + 1) 项：\(message.attachmentName ?? "")。左右划动切换，轻点打开")
        .accessibilityAddTraits(.isButton)
        .accessibilityAction(named: AppText.pick("下一项", "Next")) { stack.onMove((stack.top + 1) % stack.items.count) }
        .accessibilityAction(named: AppText.pick("上一项", "Previous")) {
            stack.onMove((stack.top - 1 + stack.items.count) % stack.items.count)
        }
        let chip = BatchChip(icon: "chevrons-up-down", title: AppText.pick("展开", "Expand"), count: stack.items.count,
                             accessibility: AppText.pick("展开全部 \(stack.items.count) 项", "Expand all \(stack.items.count)"),
                             action: stack.onExpand)
            .modifier(PillEntrance(enter: stack.enter, hidden: hidden, done: stack.onEntered))
        return HStack(spacing: 10) {
            if mine { chip; box } else { box; chip }
        }
    }

    static func stackDetail(_ message: TransferMessage) -> String {
        if message.kind == .folder {
            if let count = message.itemCount { return AppText.pick("\(count) 项", "\(count) items") }
            return ""
        }
        return message.byteSize.map { ByteCountFormatter.string(fromByteCount: $0, countStyle: .decimal) } ?? ""
    }

    /// A file or folder message is a flat card (native-rewrite.md「文件卡片」): the same on
    /// both sides, the side and the line below tell sent from received. A solo card hugs
    /// a short name; a name too long for one line wraps inside the message's width instead
    /// of pushing the card (and its button) off screen. A batch card is always 220 wide.
    @ViewBuilder private var fileCard: some View {
        let colors = StackColors(dark: oink.dark)
        if inBatch {
            fileCardBody(width: 220).fileCardPlate(colors)
        } else {
            ViewThatFits(in: .horizontal) {
                fileCardBody(width: nil).fixedSize(horizontal: true, vertical: false)
                fileCardBody(width: nil)
            }
            .fileCardPlate(colors)
        }
    }

    private func fileCardBody(width: CGFloat?) -> some View {
        FileCardContent(message: message, progress: progress, fixedWidth: width, onOpen: onOpenFile, onDownload: onDownload)
    }

    /// Send state of an own message, from the record (never assumed); a phone-to-phone
    /// send names the phone (「已发送给 X」).
    @ViewBuilder private var delivery: some View {
        let line = TransferLayout.deliveryLine(message.delivery, peer: message.isDirect ? peerName : nil, progress: progress,
                                               failure: message.deliveryFailure, chinese: AppText.pick("zh", "en") == "zh")
        HStack(spacing: 3) {
            switch message.delivery {
            case .failed: Lucide(name: "alert-circle", size: 11, color: oink.badInk)
            case .sent: Lucide(name: "check", size: 11, color: oink.hint)
            case .cancelled: Lucide(name: "x", size: 11, stroke: "2_2", color: oink.hint)
            case .sending, nil: EmptyView()
            }
            if let line { Text(line).foregroundStyle(message.delivery == .failed ? oink.badInk : oink.hint) }
        }
        .font(.oink(9).monospacedDigit())
        .foregroundStyle(oink.hint)
        .frame(minHeight: 14)
        .padding(.top, 5)
    }

    /// A phone-to-phone receive that stopped: 「对方已断开，接收失败」or「已取消」.
    @ViewBuilder private var receiveStatus: some View {
        if message.state == .stored, let peerName {
            HStack(spacing: 3) { Lucide(name: "check", size: 11, color: oink.hint); Text(AppText.pick("已发送给 \(peerName)", "Sent to \(peerName)")) }
                .font(.oink(9)).foregroundStyle(oink.hint).frame(minHeight: 14).padding(.top, 5)
        }
        if let line = TransferLayout.receiveLine(message.state, failure: message.deliveryFailure, chinese: AppText.pick("zh", "en") == "zh") {
            let failed = message.state == .failed
            HStack(spacing: 3) {
                Lucide(name: failed ? "alert-circle" : "x", size: 11, stroke: failed ? "2" : "2_2", color: failed ? oink.badInk : oink.hint)
                Text(line).foregroundStyle(failed ? oink.badInk : oink.hint)
            }
            .font(.oink(9))
            .frame(minHeight: 14)
            .padding(.top, 5)
        }
    }
}

/// Icon and caption inside an image bubble that has no picture to show
/// (expired, offline, failed); same tokens as the file card's secondary text.
private struct ImageNotice: View {
    @Environment(\.oink) private var oink
    let icon: String
    let text: String
    var body: some View {
        VStack(spacing: 6) {
            Lucide(name: icon, size: 24, color: oink.faint)
            Text(text).font(.oink(11)).foregroundStyle(oink.hint)
        }
        .accessibilityHidden(true)
    }
}

/// A tap that retries a failed fetch; absent otherwise, so text selection is untouched.
private struct RetryTap: ViewModifier {
    let enabled: Bool
    let action: () -> Void
    func body(content: Content) -> some View {
        if enabled {
            content.contentShape(Rectangle()).onTapGesture(perform: action)
                .accessibilityAddTraits(.isButton)
        } else {
            content
        }
    }
}

extension TransferMessage {
    /// Shown in a received text bubble whose body has not arrived.
    var pendingTextStatus: String {
        switch availability?.lowercased() {
        case "sourceoffline", "source_offline": return AppText.pick("来源设备离线，暂时无法获取", "Source offline, not available now")
        case "nosuchitem", "no_such_item", "evicted": return AppText.pick("内容已失效", "Content expired")
        default: break
        }
        return state == .failed ? AppText.pick("获取失败，点按重试", "Could not load, tap to retry") : AppText.pick("正在获取…", "Loading…")
    }

    /// A received text whose fetch failed (its source was unreachable) is fetched
    /// again by a tap, as a failed image is.
    var textRetryable: Bool {
        kind == .text && !isMine && text == nil && !sensitive && state == .failed
            && !["sourceoffline", "source_offline", "nosuchitem", "no_such_item", "evicted"].contains(availability?.lowercased() ?? "")
    }

    /// Tapping an image opens the viewer unless there is nothing it could
    /// show: the original is gone, or the PC is offline and no preview exists.
    var opensViewer: Bool {
        switch imagePreview {
        case .expired: false
        case .offline: false
        default: true
        }
    }
}

// MARK: - Folder

/// Folder contents (`.tr-folder-page`, native-rewrite.md「文件夹页」): back + name header,
/// rows in the file-card language 4 apart with a divider in the middle of the gap, picture
/// thumbnails, the file cards' control on files and a chevron on sub-folders.
private struct FolderPage: View {
    @EnvironmentObject private var core: CoreSession
    /// Download progress (published apart from the session).
    @ObservedObject private var transfers = CoreSession.shared.progress
    @Environment(\.oink) private var oink
    @Environment(\.dismiss) private var dismiss
    let message: TransferMessage
    @ObservedObject var model: TransferModel
    let quickLook: QuickLookPresenter
    /// Opened sub-folders, innermost last; empty = the folder's root.
    @State private var stack: [TransferDirectoryEntry] = []
    /// Downloaded children of this folder by listing entry id.
    @State private var children: [UInt64: TransferMessage] = [:]

    private var node: UInt64 { stack.last?.id ?? 0 }
    private var page: TransferDirectoryPage? {
        message.recordID.flatMap { core.transferDirectories[IOSDirectoryKey(folderMessageID: $0, directory: node)] }
    }
    /// The listing of this page (or of its next page) ended without one.
    private var failure: TransferDirectoryFailure? {
        message.recordID.flatMap { core.folderListings.failures[IOSDirectoryKey(folderMessageID: $0, directory: node)] }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 4) {
                ToolbarIconButton(label: "返回", action: { if stack.isEmpty { dismiss() } else { stack.removeLast() } }) {
                    Lucide(name: "chevron-left", size: 23, color: oink.ink)
                }
                Text(title).font(.oink(17, .bold)).foregroundStyle(oink.ink)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 16)
            .frame(height: 64)
            .overlay(alignment: .bottom) { Hairline(color: oink.lineSolid) }
            if let failure, page?.entries.isEmpty ?? true {
                FolderFailureState(failure: failure) { request(more: false) }
                    .frame(maxHeight: .infinity)
            } else {
                ScrollView {
                    VStack(spacing: 0) {
                        let entries = page?.entries ?? []
                        ForEach(Array(entries.enumerated()), id: \.element.id) { index, entry in
                            // Rows 4 apart; the 1 px line sits in the middle of the gap, from the
                            // text's left edge (36 + 12) to the right edge.
                            if index > 0 { Hairline(color: oink.lineSolid).padding(.leading, 48).frame(height: 4) }
                            row(entry)
                        }
                        // A failed next page shows why below the entries already listed.
                        if let failure {
                            FolderFailureState(failure: failure) { request(more: true) }
                        } else if page?.hasMore == true {
                            Button("加载更多") { request(more: true) }.font(.oink(13)).foregroundStyle(oink.muted).padding(.vertical, 12)
                        }
                    }
                    .padding(.horizontal, 16).padding(.top, 8).padding(.bottom, 16)
                }
            }
            Spacer(minLength: 0)
        }
        .background(oink.bg)
        .task(id: node) { if page == nil { request(more: false) } }
        .onAppear(perform: loadChildren)
        .onChange(of: core.transferChange) { _, _ in loadChildren() }
        .onChange(of: children) { _, _ in quickLook.refresh() }
        // A file of this level that finished downloading opens with its level's sequence.
        .onAppear {
            quickLook.folderOpener = { file in
                guard let child = children.values.first(where: { $0.recordID == file.id }) else { return false }
                return quickLook.open(child.id, in: quickLookSource)
            }
        }
        .onDisappear { quickLook.folderOpener = nil }
        // Leaving the folder must not open a file the user no longer waits for.
        .onDisappear { core.cancelTransferAutoOpen() }
    }

    private var title: String { stack.last?.name ?? message.attachmentName ?? "文件夹" }

    private func loadChildren() {
        #if DEBUG
        if DesignScene.current != nil { children = Self.designChildren(message); return }
        #endif
        do { children = try core.folderChildren(message) } catch { model.errorMessage = error.oinkMessage }
    }

    #if DEBUG
    /// The design states' downloaded files of the folder (CoreSession lists its entries and
    /// sets the progress of 9402 / 9403): transfer-folder and transfer-folder-states.
    private static func designChildren(_ folder: TransferMessage) -> [UInt64: TransferMessage] {
        func child(_ id: Int, node: UInt64, _ kind: TransferMessageKind, _ name: String, _ state: AttachmentState,
                   _ status: TransferDownloadStatus, art: String? = nil, thumbnail: String? = nil) -> TransferMessage {
            var child = TransferMessage(id: String(id), createdAt: folder.createdAt, senderDeviceID: folder.senderDeviceID,
                            senderName: folder.senderName, offerID: nil, sourceBoot: nil, operationID: nil, itemID: nil,
                            representationID: nil, originSenderID: folder.originSenderID, proxyDeviceID: nil, proxyName: nil,
                            kind: kind, text: nil, contentID: art ?? "design-child-\(id)", attachmentName: name, byteSize: nil,
                            uti: nil, state: state, availability: "Ready", isMine: false, parentID: folder.recordID, directoryNode: node)
            child.downloadStatus = status
            child.thumbnailPath = thumbnail
            return child
        }
        switch DesignScene.current {
        case "transfer-folder":
            return [1: child(9401, node: 1, .image, "界面预览.png", .stored, .downloaded, art: "fixture-batch-20"),
                    2: child(9402, node: 2, .file, "设计说明.pdf", .downloading, .downloading)]
        case "transfer-folder-states":
            // 现场 02.jpg is not downloaded: its record holds only the computer's preview.
            return [1: child(9411, node: 1, .image, "现场 01.jpg", .stored, .downloaded, art: "fixture-batch-20"),
                    2: child(9412, node: 2, .image, "现场 02.jpg", .placeholder, .notDownloaded, thumbnail: "fixture-batch-21"),
                    3: child(9403, node: 3, .file, "验收单.pdf", .downloading, .downloading),
                    5: child(9415, node: 5, .file, "材料清单.xlsx", .stored, .downloaded),
                    6: child(9416, node: 6, .file, "原始照片.zip", .failed, .downloadFailed)]
        default:
            return [:]
        }
    }
    #endif

    /// 0–100 of a file entry: live progress of its child record, 100 once stored.
    private func progress(_ entry: TransferDirectoryEntry) -> Int? {
        guard let child = record(of: entry) else { return nil }
        if let id = child.recordID, let fraction = transfers.records[id]?.fraction { return min(99, Int((fraction * 100).rounded())) }
        switch child.state {
        case .stored: return 100
        case .downloading: return 0
        default: return nil
        }
    }

    private static func isFolder(_ entry: TransferDirectoryEntry) -> Bool { entry.kind == "directory" || entry.kind == "folder" }

    /// The entry's record: as the folder's children load now, else the listing's snapshot.
    /// Having one does not mean downloaded (a picture asked for its thumbnail has one too).
    private func record(of entry: TransferDirectoryEntry) -> TransferMessage? { children[entry.id] ?? entry.child }

    /// A file's control and second line, as on a file card (one rule for all three places).
    /// A file without a record is not downloaded.
    private func cardLook(_ entry: TransferDirectoryEntry) -> TransferLayout.FileCardLook {
        TransferLayout.fileCard(kind: .file, mine: false, status: record(of: entry)?.downloadStatus ?? .notDownloaded,
                                delivery: nil, progress: progress(entry))
    }

    private func row(_ entry: TransferDirectoryEntry) -> some View {
        let folder = Self.isFolder(entry)
        let look = folder ? TransferLayout.FileCardLook() : cardLook(entry)
        let picture = folder ? nil : thumbnail(entry)
        return HStack(spacing: 8) {
            Button {
                if folder { stack.append(entry) } else if !look.inert { open(entry) }
            } label: {
                HStack(spacing: 12) {
                    leading(entry, picture: picture, folder: folder, grey: look.greyTile)
                    FileCardText(name: entry.name, ext: folder ? nil : TransferLayout.fileExtension(entry.name),
                                 detail: look.reason.map(FileCardContent.reasonText) ?? detail(entry, folder: folder),
                                 reasonBad: look.reason == .downloadFailed, large: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            if folder {
                // Only on the folder page: the folder cards in the conversation have none.
                Lucide(name: "chevron-right", size: 18, color: oink.faint).frame(width: 40, height: 44).accessibilityHidden(true)
            } else if let slot = look.slot {
                FileSlotButton(slot: slot, name: entry.name) { download(entry, open: false) }
            }
        }
        .padding(.vertical, 10)
        .frame(minHeight: 64)
        // A computer's picture without a preview yet: ask for one, as a picture message does.
        .onAppear { if !folder, picture == nil, TransferLayout.isPictureName(entry.name) { requestThumbnail(entry) } }
    }

    /// 36 × 36: a picture's thumbnail (cover, 0.5 pt `--c-media-edge`), otherwise the
    /// type tile (a picture without a thumbnail yet shows the file tile until it comes).
    @ViewBuilder private func leading(_ entry: TransferDirectoryEntry, picture: ListPicture?, folder: Bool, grey: Bool) -> some View {
        if let picture {
            let shape = RoundedRectangle(cornerRadius: 10, style: .continuous)
            // 36 pt at 3x, a little over for the cover crop.
            ListPictureView(picture, maxPixel: 160) { FileTypeTile(name: entry.name, folder: false, grey: grey) }
                .frame(width: 36, height: 36)
                .clipShape(shape)
                .contentShape(shape)
                .mediaEdge(shape)
                .accessibilityHidden(true)
        } else {
            FileTypeTile(name: entry.name, folder: folder, grey: grey)
        }
    }

    /// A picture's thumbnail: its downloaded file (every file of a phone-to-phone folder is
    /// here already, so the thumbnail is made from it on this phone), else the computer's
    /// preview on the entry's record (`transfer_request_directory_thumbnail`).
    private func thumbnail(_ entry: TransferDirectoryEntry) -> ListPicture? {
        let child = record(of: entry)
        guard TransferLayout.isPictureName(entry.name) || child?.kind == .image else { return nil }
        #if DEBUG
        if let art = child?.thumbnailPath, art.hasPrefix("fixture-batch-") { return .fixture(FixturePreviewImage.image(for: art)) }
        if let child, child.downloadStatus == .downloaded, let art = child.contentID, art.hasPrefix("fixture-batch-") {
            return .fixture(FixturePreviewImage.image(for: art))
        }
        #endif
        if let child, child.downloadStatus == .downloaded, let path = child.localPath { return .file(path) }
        return child?.thumbnailPath.map { .file($0) }
    }

    private func requestThumbnail(_ entry: TransferDirectoryEntry) {
        #if DEBUG
        if DesignScene.current != nil { return }
        #endif
        core.requestDirectoryThumbnail(message, entry: entry)
    }

    /// A file's size; a sub-folder's file count when the listing has it.
    private func detail(_ entry: TransferDirectoryEntry, folder: Bool) -> String {
        if folder { return entry.fileCount.map { AppText.pick("\($0) 个文件", "\($0) files") } ?? AppText.pick("文件夹", "Folder") }
        return entry.size.map { ByteCountFormatter.string(fromByteCount: Int64($0), countStyle: .decimal) } ?? ""
    }

    private func request(more: Bool) {
        #if DEBUG
        if DesignScene.current != nil { return }
        #endif
        core.requestTransferDirectory(message, node: node, more: more)
    }

    /// Tap a file: Quick Look on it, paging through this level's files (user 2026-11-11),
    /// when it is here; otherwise download it and it opens then.
    private func open(_ entry: TransferDirectoryEntry) {
        if let child = children[entry.id], quickLook.open(child.id, in: quickLookSource) {
            Haptics.selection()
            return
        }
        download(entry, open: true)
    }

    /// This level's files in list order (sub-folders are not in it).
    private var quickLookSource: QuickLookPresenter.Source {
        let files = (page?.entries ?? []).filter { !Self.isFolder($0) }
        let children = children
        let entries = files.compactMap { entry in
            children[entry.id].flatMap {
                QuickLookSequence.entry($0, original: QuickLookSequence.storedFile, thumbnail: QuickLookSequence.thumbnailFile,
                                        canPreview: TransferView.canPreview)
            }
        }
        let core = core
        return QuickLookPresenter.Source(
            entries: entries,
            resolve: { [weak core] id in
                // The level's children as they are now (a stand-in turns into its original).
                guard let core else { return nil }
                let now = (try? core.folderChildren(self.message)) ?? children
                return now.values.first { $0.id == id }.flatMap {
                    QuickLookSequence.entry($0, original: QuickLookSequence.storedFile, thumbnail: QuickLookSequence.thumbnailFile,
                                            canPreview: TransferView.canPreview)
                }
            },
            onStandInShown: { [weak core] id in
                guard let core, let file = files.first(where: { children[$0.id]?.id == id }),
                      children[file.id]?.state != .downloading else { return }
                try? core.downloadDirectoryFile(self.message, entry: file, open: false)
            })
    }

    /// Tap = download and open with QuickLook; the button only downloads. The
    /// file becomes a child record of the folder, never a new message.
    private func download(_ entry: TransferDirectoryEntry, open: Bool) {
        Haptics.selection()
        do {
            try core.downloadDirectoryFile(message, entry: entry, open: open)
            loadChildren()
        } catch { model.errorMessage = error.oinkMessage }
    }
}

/// A folder listing that ended without a page (`DevicesEmptyState` layout):
/// why, and 重试 when it can help.
struct FolderFailureState: View {
    @Environment(\.oink) private var oink
    let failure: TransferDirectoryFailure
    let retry: () -> Void

    var body: some View {
        VStack(spacing: 0) {
            Lucide(name: "alert-circle", size: 24, color: oink.badInk)
                .frame(width: 56, height: 56)
                .background(oink.badSoft, in: Circle())
            Text(failure.text)
                .font(.oink(13.5)).foregroundStyle(oink.muted).multilineTextAlignment(.center)
                .lineSpacing(13.5 * 0.6 - 4).frame(maxWidth: 280).padding(.top, 14)
            if failure.retryable {
                Button(AppText.pick("重试", "Retry"), action: retry)
                    .buttonStyle(CapsuleButtonStyle(kind: .primary))
                    .padding(.top, 16)
            }
        }
        .padding(.horizontal, 32).padding(.vertical, 24)
        .frame(maxWidth: .infinity)
    }
}

// MARK: - Camera

/// System camera for 拍摄; returns a JPEG file.
private struct CameraPicker: UIViewControllerRepresentable {
    let done: (URL?) -> Void
    func makeCoordinator() -> Coordinator { Coordinator(done: done) }
    func makeUIViewController(context: Context) -> UIImagePickerController {
        let picker = UIImagePickerController()
        picker.sourceType = UIImagePickerController.isSourceTypeAvailable(.camera) ? .camera : .photoLibrary
        picker.delegate = context.coordinator
        return picker
    }
    func updateUIViewController(_ controller: UIImagePickerController, context: Context) {}
    final class Coordinator: NSObject, UIImagePickerControllerDelegate, UINavigationControllerDelegate {
        let done: (URL?) -> Void
        init(done: @escaping (URL?) -> Void) { self.done = done }
        func imagePickerController(_ picker: UIImagePickerController, didFinishPickingMediaWithInfo info: [UIImagePickerController.InfoKey: Any]) {
            guard let image = info[.originalImage] as? UIImage, let data = image.jpegData(compressionQuality: 0.9) else { done(nil); return }
            let url = FileManager.default.temporaryDirectory.appendingPathComponent("oink-\(UUID().uuidString).jpg")
            try? data.write(to: url)
            done(url)
        }
        func imagePickerControllerDidCancel(_ picker: UIImagePickerController) { done(nil) }
    }
}

#if DEBUG
/// The mockup's placeholder picture (TransferPage.jsx `PreviewImage`), for the transfer fixtures.
enum FixturePreviewImage {
    static let image = render(sky: UIColor(red: 0xE8 / 255, green: 0xE8 / 255, blue: 0xEC / 255, alpha: 1))

    /// Batch fixtures (`fixture-batch-<n>`): the transfer-stack prototype's photo art
    /// (TransferBatch.jsx `Art`, palette + aspect of each demo item).
    private static let batch: [Int: (palette: [UInt32], aspect: CGFloat)] = {
        let meadow: [UInt32] = [0xE8ECE4, 0xFFB8C9, 0xB5C6B5, 0x8DA68D], dusk: [UInt32] = [0xF6DCCB, 0xFF9F7A, 0xD39A86, 0xA8707A]
        let lake: [UInt32] = [0xD8E6EF, 0xFFF1C2, 0x9FB8C8, 0x6F90A6], night: [UInt32] = [0x2B3350, 0xF6E7B0, 0x46507A, 0x323A5E]
        let sand: [UInt32] = [0xF3EAD6, 0xFFD28A, 0xE0C48F, 0xC9A46B], berry: [UInt32] = [0xEFE2F0, 0xFF8FAB, 0xC3A6CF, 0x8F73A3]
        return [20: (meadow, 3 / 4), 21: (lake, 4 / 3), 22: (dusk, 16 / 9), 23: (night, 9 / 16), 24: (sand, 1),
                25: (berry, 4 / 5), 26: (lake, 21 / 9), 27: (meadow, 3 / 2), 31: (sand, 3 / 4)]
    }()
    private static var rendered: [Int: UIImage] = [:]

    static func image(for contentID: String?) -> UIImage {
        guard let contentID, contentID.hasPrefix("fixture-batch-"), let n = Int(contentID.split(separator: "-").last ?? ""),
              let art = batch[n] else { return image }
        if let done = rendered[n] { return done }
        let made = render(art.palette, aspect: art.aspect)
        rendered[n] = made
        return made
    }

    /// The prototype art: a 300 × 300 scene drawn "slice" (cover) into the aspect.
    private static func render(_ palette: [UInt32], aspect: CGFloat) -> UIImage {
        func color(_ hex: UInt32) -> UIColor {
            UIColor(red: CGFloat(hex >> 16) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255, blue: CGFloat(hex & 0xFF) / 255, alpha: 1)
        }
        let size = aspect >= 1 ? CGSize(width: 600, height: 600 / aspect) : CGSize(width: 600 * aspect, height: 600)
        return UIGraphicsImageRenderer(size: size).image { context in
            let c = context.cgContext
            let scale = max(size.width, size.height) / 300
            c.translateBy(x: (size.width - 300 * scale) / 2, y: (size.height - 300 * scale) / 2)
            c.scaleBy(x: scale, y: scale)
            color(palette[0]).setFill(); c.fill(CGRect(x: 0, y: 0, width: 300, height: 300))
            color(palette[1]).setFill(); c.fillEllipse(in: CGRect(x: 184, y: 62, width: 60, height: 60))
            color(palette[2]).setFill()
            c.move(to: CGPoint(x: -20, y: 230)); c.addLine(to: CGPoint(x: 80, y: 120)); c.addLine(to: CGPoint(x: 176, y: 240))
            c.addLine(to: CGPoint(x: 236, y: 178)); c.addLine(to: CGPoint(x: 326, y: 258)); c.addLine(to: CGPoint(x: 326, y: 300))
            c.addLine(to: CGPoint(x: -20, y: 300)); c.closePath(); c.fillPath()
            color(palette[3]).setFill()
            c.move(to: CGPoint(x: -20, y: 300)); c.addLine(to: CGPoint(x: 120, y: 204)); c.addLine(to: CGPoint(x: 250, y: 300)); c.closePath(); c.fillPath()
        }
    }

    private static func render(sky: UIColor) -> UIImage {
        let size = CGSize(width: 520, height: 292)
        return UIGraphicsImageRenderer(size: size).image { context in
            let c = context.cgContext
            c.scaleBy(x: 2, y: 2)
            sky.setFill()
            c.fill(CGRect(x: 0, y: 0, width: 260, height: 146))
            UIColor(red: 1, green: 0xB8 / 255, blue: 0xC9 / 255, alpha: 1).setFill()
            c.fillEllipse(in: CGRect(x: 194 - 21, y: 43 - 21, width: 42, height: 42))
            UIColor(red: 0xB5 / 255, green: 0xC6 / 255, blue: 0xB5 / 255, alpha: 1).setFill()
            c.move(to: CGPoint(x: 0, y: 118)); c.addLine(to: CGPoint(x: 67, y: 42)); c.addLine(to: CGPoint(x: 141, y: 129))
            c.addLine(to: CGPoint(x: 183, y: 84)); c.addLine(to: CGPoint(x: 260, y: 146)); c.addLine(to: CGPoint(x: 0, y: 146)); c.closePath(); c.fillPath()
            UIColor(red: 0x8D / 255, green: 0xA6 / 255, blue: 0x8D / 255, alpha: 1).setFill()
            c.move(to: CGPoint(x: 0, y: 144)); c.addLine(to: CGPoint(x: 105, y: 76)); c.addLine(to: CGPoint(x: 192, y: 146)); c.closePath(); c.fillPath()
        }
    }
}
#endif
