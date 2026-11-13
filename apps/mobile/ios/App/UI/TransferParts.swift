import SwiftUI
import UIKit

// Transfer page parts:
// the folded long text and the batch stack with its cards and chips.

/// Motion of the transfer page. The stack's swipe, expand and collapse follow the Android
/// implementation (TransferLayout's stack motion); the text fold keeps the spec's iOS curve.
enum TransferMotion {
    /// 查看更多: 300 ms cubic-bezier(.32,.72,0,1).
    static let text = Animation.timingCurve(0.32, 0.72, 0, 1, duration: 0.3)
    /// Rows sliding while a batch expands or collapses: 400 ms standard easing.
    static let rows = Animation.timingCurve(0.2, 0, 0, 1, duration: TransferLayout.morphMs / 1000)
    /// A swipe let go short of a switch springs back: 250 ms cubic-bezier(0,0,0,1).
    static let cancel = Animation.timingCurve(0, 0, 0, 1, duration: TransferLayout.swipeCancelMs / 1000)
    /// Reduce Motion: every change is a 150 ms cross fade.
    static let reduced = Animation.easeInOut(duration: 0.15)
}

/// The `--c-stack-*` colours of the spec (§5), light / dark; the file cards (single
/// messages, expanded batch items, folder rows) use them too (native-rewrite.md「文件卡片」).
struct StackColors {
    let card, edge, ink, hint, mediaEdge: Color
    let dim: Double
    let shadowNear, shadowFar: Color
    let shadowFarRadius, shadowFarY: CGFloat
    /// `--c-stack-card-edge` is a 1 pt inner stroke (user 2026-11-11: 0.5 pt was too faint
    /// once the flat cards lost their shadow).
    static let edgeWidth: CGFloat = 1

    init(dark: Bool) {
        card = Color(hex: dark ? 0x2D323E : 0xFFFFFF)
        // Ink 5 % / #ECEEF2 4 % (file-bubble SPEC §1.2).
        edge = dark ? Color(hex: 0xECEEF2).opacity(0.04) : Color(hex: 0x1B1C22).opacity(0.05)
        ink = Color(hex: dark ? 0xECEEF2 : 0x1B1C22)
        hint = Color(hex: dark ? 0x868D9D : 0xA3A5AE)
        // `--c-media-edge` (user 2026-11-12): the 0.5 pt hairline inside every picture of the
        // transfer page (single pictures, stack cards, folder thumbnails); ink 10 % / white 12 %.
        mediaEdge = dark ? Color.white.opacity(0.12) : Color(hex: 0x1B1C22).opacity(0.10)
        dim = dark ? 0.16 : 0
        shadowNear = dark ? .black.opacity(0.4) : Color(hex: 0x1B1C22).opacity(0.08)
        shadowFar = dark ? .black.opacity(0.5) : Color(hex: 0x1B1C22).opacity(0.12)
        // CSS blur ÷ 2: 0 4 12 → radius 6 (light), 0 6 16 → radius 8 (dark).
        shadowFarRadius = dark ? 8 : 6
        shadowFarY = dark ? 6 : 4
    }

    /// `color-mix(in srgb, color fraction, --c-stack-card)`: an opaque tint of the card.
    func tint(_ color: Color, _ fraction: Double) -> Color {
        card.mix(with: color, by: fraction, in: .device)
    }

    /// The progress ring's track on a card: ink 12 % over the card (in dark mode
    /// `--c-progress-track` is the card's own colour).
    var ringTrack: Color { tint(ink, 0.12) }
}

extension View {
    /// `--c-stack-shadow`: two layers, scaled by `strength` (the morph fades it).
    func stackShadow(_ colors: StackColors, strength: Double = 1) -> some View {
        shadow(color: colors.shadowNear.opacity(strength), radius: 1, y: 1)
            .shadow(color: colors.shadowFar.opacity(strength), radius: colors.shadowFarRadius, y: colors.shadowFarY)
    }
}

// MARK: - Pictures

/// What a list picture shows: a downloaded file, decoded off the main thread
/// (ImageThumbnails), or the art of a DEBUG design state.
enum ListPicture {
    case file(String)
    case fixture(UIImage)

    /// Width / height, known before the picture is decoded (layout never waits for it).
    var aspect: Double? {
        switch self {
        case .file(let path): ImageThumbnails.pixelSize(path: path).map { Double($0.width / $0.height) }
        case .fixture(let image): Double(image.size.width / image.size.height)
        }
    }
}

/// A list picture filling its frame: the cached thumbnail at once, otherwise
/// `placeholder` until the background decode is done.
struct ListPictureView<Placeholder: View>: View {
    let picture: ListPicture
    /// Long side decoded (a folder row's 36 pt square needs far less than a message).
    let maxPixel: Int
    @ViewBuilder let placeholder: () -> Placeholder
    @State private var decoded: UIImage?

    init(_ picture: ListPicture, maxPixel: Int = ImageThumbnails.listMaxPixel, @ViewBuilder placeholder: @escaping () -> Placeholder) {
        self.picture = picture
        self.maxPixel = maxPixel
        self.placeholder = placeholder
        if case .file(let path) = picture {
            let cached = ImageThumbnails.cached(path: path, maxPixel: maxPixel)
            _decoded = State(initialValue: cached)
            // Rows are built ahead of the scroll: start decoding before the row is on screen.
            if cached == nil { ImageThumbnails.prefetch(path: path, maxPixel: maxPixel) }
        }
    }

    var body: some View {
        switch picture {
        case .fixture(let image):
            Image(uiImage: image).resizable().scaledToFill()
        case .file(let path):
            Group {
                if let decoded { Image(uiImage: decoded).resizable().scaledToFill() } else { placeholder() }
            }
            .task(id: path) {
                decoded = ImageThumbnails.cached(path: path, maxPixel: maxPixel)
                if decoded == nil { decoded = await ImageThumbnails.load(path: path, maxPixel: maxPixel) }
            }
        }
    }
}

// MARK: - Long text

/// A text bubble's body showing `shown` lines; longer text is clipped there with
/// the last three lines fading (alpha mask, SPEC §1.2). `lines` reports the
/// laid-out line count (wrapped lines, not paragraphs), at most `shown` + 1. The height animates with
/// the caller's transaction; when everything is shown the fade goes to zero.
struct LongTextBody: View {
    let text: String
    let shown: Int
    let color: Color
    @Binding var lines: Int?
    /// 长按「选择」: the system selection replaces the drawn text, line on line.
    var selecting = false
    var onEndSelect: () -> Void = {}
    @State private var fullHeight: CGFloat = 0
    @State private var lineHeight: CGFloat = 0
    @State private var twoLineHeight: CGFloat = 0

    static let font = Font.oink(14)
    static let uiFont = UIFont.systemFont(ofSize: 14)
    /// Line pitch ≈ 1.65 × 14 = 23.1 (SPEC §1.1) on the system font's line box.
    static let lineSpacing: CGFloat = 6.4

    /// Distance from one line's top to the next (measured, so CJK fallback fonts count).
    private var pitch: CGFloat { twoLineHeight - lineHeight }
    private var folded: Bool { (lines ?? 0) > shown }
    private var visibleHeight: CGFloat { folded ? lineHeight + CGFloat(shown - 1) * pitch : fullHeight }

    var body: some View {
        // Laid out and drawn up to one line past the shown ones, not the whole text: a
        // 70-line note was typeset and rasterised in full (on the main thread, at commit)
        // each time its row was built while scrolling, to show 10 lines. The extra line
        // tells whether more follows (`lines` then counts `shown` + 1); it and its
        // truncation mark lie below the clip.
        Text(text)
            .font(Self.font).foregroundStyle(color).lineSpacing(Self.lineSpacing)
            .lineLimit(shown + 1)
            .accessibilityLabel(text)
            .fixedSize(horizontal: false, vertical: true)
            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { fullHeight = $0; count() }
            .opacity(selecting ? 0 : 1)
            .overlay(alignment: .top) {
                // The whole text, at full height, clipped with the fold below.
                if selecting {
                    SelectableText(text: text, color: UIColor(color), lineHeight: lineHeight, pitch: pitch, onEnd: onEndSelect)
                }
            }
            .frame(height: fullHeight > 0 ? visibleHeight : nil, alignment: .top)
            .clipped()
            .mask { FoldMask(line: pitch, strength: folded ? 1 : 0) }
            .background(alignment: .topLeading) {
                ZStack(alignment: .topLeading) {
                    Text("国").onGeometryChange(for: CGFloat.self) { $0.size.height } action: { lineHeight = $0; count() }
                    Text("国\n国").onGeometryChange(for: CGFloat.self) { $0.size.height } action: { twoLineHeight = $0; count() }
                }
                .font(Self.font).lineSpacing(Self.lineSpacing).fixedSize().hidden()
            }
    }

    private func count() {
        guard lineHeight > 0, pitch > 0, fullHeight > 0 else { return }
        let value = Int(((fullHeight - lineHeight) / pitch).rounded()) + 1
        if value != lines { lines = value }
    }

    /// Paragraph style that lays UIKit lines exactly on the SwiftUI ones (for 选择).
    static func paragraph(lineHeight: CGFloat, pitch: CGFloat) -> NSParagraphStyle {
        let style = NSMutableParagraphStyle()
        style.minimumLineHeight = lineHeight
        style.maximumLineHeight = lineHeight
        style.lineSpacing = max(0, pitch - lineHeight)
        return style
    }
}

/// The fold's alpha mask: opaque, then from three lines above the bottom
/// 1 → 0.66 → 0.30 → 0 (TransferLayout.fadeStops). `strength` 0 is fully
/// opaque; it animates to 0 when the last 查看更多 shows everything.
struct FoldMask: View {
    let line: CGFloat
    let strength: Double
    var body: some View {
        GeometryReader { proxy in
            let height = max(proxy.size.height, 1)
            ZStack {
                LinearGradient(stops: [Gradient.Stop(color: .black, location: 0)]
                               + TransferLayout.fadeStops.map { stop in
                                   Gradient.Stop(color: .black.opacity(stop.alpha),
                                                 location: max(0, min(1, (height - line * stop.lines) / height)))
                               },
                               startPoint: .top, endPoint: .bottom)
                // Over the gradient: alpha = lerp(1, gradient, strength).
                Color.black.opacity(1 - strength)
            }
        }
    }
}

/// 查看更多: a 36 pt row across the bubble bottom, label + chevron centred (SPEC §1.3).
/// 显示 of a hidden sensitive text uses the same row.
struct MoreRow: View {
    @Environment(\.oink) private var oink
    var title = AppText.pick("查看更多", "Show more")
    let action: () -> Void
    var body: some View {
        Button(action: action) {
            HStack(spacing: 2) {
                Text(title).font(.oink(13, .semibold)).kerning(0.2)
                Lucide(name: "chevron-down", size: 15, stroke: "2_2", color: oink.accentInk).padding(.top, 1)
            }
            .foregroundStyle(oink.accentInk)
            .frame(maxWidth: .infinity, minHeight: 36)
            .contentShape(Rectangle())
        }
        .buttonStyle(PressFadeStyle())
    }
}

/// iOS press feedback of 查看更多: opacity 0.55.
private struct PressFadeStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label.opacity(configuration.isPressed ? 0.55 : 1)
    }
}

/// The system text selection on a text bubble (长按菜单「选择」): a read-only
/// UITextView laid over the bubble's text with the same lines, the whole text in
/// it and clipped to the bubble's (folded) frame. It selects everything on start;
/// clearing the selection (a tap inside) ends it.
struct SelectableText: UIViewRepresentable {
    let text: String
    let color: UIColor
    /// The SwiftUI text's measured line box and pitch, so UIKit lays the same lines.
    let lineHeight: CGFloat
    let pitch: CGFloat
    let onEnd: () -> Void
    /// Rich text (RichSelection): shown as it is, in place of the drawn blocks, at its own
    /// height; `text`, `color`, `lineHeight` and `pitch` are not used then.
    var attributed: NSAttributedString? = nil

    func makeCoordinator() -> Coordinator { Coordinator(onEnd: onEnd) }

    func makeUIView(context: Context) -> UITextView {
        let view = SelectionView()
        // Back in a window (the page was under another tab, the folder page or a cover): the
        // text view lost first responder with the window and its selection with it, though
        // the page still has the text selected; select the same range again.
        view.returned = { [weak coordinator = context.coordinator] view in
            guard let coordinator, coordinator.selecting else { return }
            view.becomeFirstResponder()
            view.selectedRange = coordinator.range
        }
        view.isEditable = false
        view.isSelectable = true
        view.isScrollEnabled = false
        view.clipsToBounds = true
        view.backgroundColor = .clear
        view.textContainerInset = .zero
        view.textContainer.lineFragmentPadding = 0
        view.delegate = context.coordinator
        view.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        view.setContentHuggingPriority(.defaultLow, for: .horizontal)
        return view
    }

    func updateUIView(_ view: UITextView, context: Context) {
        context.coordinator.onEnd = onEnd
        let style = LongTextBody.paragraph(lineHeight: lineHeight, pitch: pitch)
        let attributed = self.attributed
            ?? NSAttributedString(string: text, attributes: [.font: LongTextBody.uiFont, .foregroundColor: color, .paragraphStyle: style])
        if view.attributedText != attributed { view.attributedText = attributed }
        guard !context.coordinator.started else { return }
        context.coordinator.started = true
        DispatchQueue.main.async {
            view.becomeFirstResponder()
            view.selectedRange = NSRange(location: 0, length: (view.text as NSString).length)
            context.coordinator.range = view.selectedRange
            context.coordinator.selecting = true
            // Show the system edit menu (拷贝 / 查找 / 共享 ...) for the selection at once.
            if let menu = view.interactions.compactMap({ $0 as? UIEditMenuInteraction }).first {
                menu.presentEditMenu(with: UIEditMenuConfiguration(identifier: nil, sourcePoint: CGPoint(x: view.bounds.midX, y: 0)))
            }
        }
    }

    func sizeThatFits(_ proposal: ProposedViewSize, uiView: UITextView, context: Context) -> CGSize? {
        // Rich text: the proposed width, its own height.
        if attributed != nil, let width = proposal.width, width.isFinite {
            return CGSize(width: width, height: uiView.sizeThatFits(CGSize(width: width, height: .greatestFiniteMagnitude)).height)
        }
        // Laid over the whole text: take exactly its frame.
        return CGSize(width: proposal.width ?? 0, height: proposal.height ?? 0)
    }

    static func dismantleUIView(_ view: UITextView, coordinator: Coordinator) {
        coordinator.selecting = false
        view.resignFirstResponder()
    }

    final class Coordinator: NSObject, UITextViewDelegate {
        var onEnd: () -> Void
        var started = false
        var selecting = false
        /// The selection as the user left it (handles moved), restored on returning.
        var range = NSRange(location: 0, length: 0)
        init(onEnd: @escaping () -> Void) { self.onEnd = onEnd }
        func textViewDidChangeSelection(_ view: UITextView) {
            guard selecting else { return }
            if view.selectedRange.length > 0 {
                range = view.selectedRange
                return
            }
            // Cleared by the user (a tap inside) ends the selection; cleared because the text
            // view is leaving its window (the page goes away) does not.
            guard view.window != nil, view.isFirstResponder else { return }
            selecting = false
            onEnd()
        }
    }

    /// The text view telling when it is back in a window.
    final class SelectionView: UITextView {
        var returned: ((UITextView) -> Void)?
        override func didMoveToWindow() {
            super.didMoveToWindow()
            if window != nil { returned?(self) }
        }
    }
}

// MARK: - Chips

/// 展开 N / 收起 (SPEC §2.3, §3.1): 28 high, chevrons icon, label, optional count;
/// the hit area reaches 8 pt above and below and 4 pt to the sides.
struct BatchChip: View {
    @Environment(\.oink) private var oink
    let icon: String
    let title: String
    var count: Int? = nil
    let accessibility: String
    let action: () -> Void
    var body: some View {
        Button(action: action) {
            HStack(spacing: 3) {
                Lucide(name: icon, size: 14, color: oink.text2)
                Text(title).foregroundStyle(oink.text2)
                if let count { Text("\(count)").monospacedDigit().foregroundStyle(oink.muted).padding(.leading, 1) }
            }
            .font(.oink(12, .semibold)).lineLimit(1).fixedSize()
            .padding(.leading, 8).padding(.trailing, 10).frame(height: 28)
            .background(oink.chip, in: Capsule())
            .padding(.vertical, 8).padding(.horizontal, 4)
            .contentShape(Rectangle())
        }
        .buttonStyle(ShrinkButtonStyle(scale: 0.95))
        .padding(.vertical, -8).padding(.horizontal, -4)
        .accessibilityLabel(accessibility)
    }
}

// MARK: - Batch stack

/// A collapsed batch (SPEC §2.2): the top card in front, the others around it circularly
/// with their peek, tilt and dimming. A horizontal swipe moves the cards with the finger;
/// letting go past 40 pt (or fast) brings the next (left) or previous (right) card to the
/// top, circularly, otherwise the cards spring back (Android TransferBatch.kt). A tap is the
/// caller's. `card` draws one item's face at its contain size inside the 120 × 160 box.
struct BatchStack<Card: View>: View {
    let ids: [String]
    let top: Int
    /// DEBUG design state only: a drag held at this dx.
    var demoDrag: CGFloat? = nil
    let onMove: (Int) -> Void
    @ViewBuilder let card: (Int) -> Card
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.oink) private var oink
    /// The finger's horizontal travel (0 at rest).
    @State private var drag: Double = 0
    /// A switch in progress and its time (0…1, animated linearly).
    @State private var settling: TransferLayout.StackSettle?
    @State private var settle: Double = 0

    private var count: Int { ids.count }

    var body: some View {
        let box = TransferLayout.stackBox
        let roles = settling?.roles ?? TransferLayout.roles(count: count, top: top)
        let dim = StackColors(dark: oink.dark).dim
        ZStack {
            ForEach(roles.sorted { $0.value.pose.z < $1.value.pose.z }, id: \.key) { entry in
                card(entry.key)
                    .frame(width: box.width, height: box.height)
                    .modifier(StackCardMotion(drag: drag, settle: settle, index: entry.key, role: entry.value, count: count,
                                              settling: settling, dim: dim))
                    .accessibilityHidden(entry.value != .top)
            }
        }
        .frame(width: box.width, height: box.height)
        .contentShape(Rectangle())
        .onAppear { if let demoDrag { drag = demoDrag } }
        // Horizontal only; a vertical drag that starts on the stack scrolls the list.
        .gesture(HorizontalPan(onChanged: { dx in
            guard count > 1, settling == nil else { return }
            drag = dx
        }, onEnded: { dx, velocity in
            guard count > 1, settling == nil, drag != 0 else { return }
            let next = TransferLayout.swipe(top: top, count: count, dx: dx, dy: 0, velocity: velocity)
            if next == top {
                withAnimation(reduceMotion ? TransferMotion.reduced : TransferMotion.cancel) { drag = 0 }
            } else {
                commit(direction: dx < 0 || (dx == 0 && velocity < 0) ? 1 : -1)
            }
        }))
    }

    /// Switch: every card goes from where the finger left it to its new place in 320 ms.
    private func commit(direction: Int) {
        Haptics.selection()
        let plan = TransferLayout.settle(count: count, top: top, dx: drag, direction: direction)
        if reduceMotion {
            withAnimation(TransferMotion.reduced) { drag = 0; onMove(plan.newTop) }
            return
        }
        var still = Transaction()
        still.disablesAnimations = true
        withTransaction(still) {
            settling = plan
            drag = 0
        }
        withAnimation(.linear(duration: TransferLayout.swipeCommitMs / 1000)) {
            settle = 1
        } completion: {
            withTransaction(still) {
                onMove(plan.newTop)
                settling = nil
                settle = 0
            }
        }
    }
}

/// One card's pose, evaluated on every frame while the finger moves, a swipe springs back
/// (drag animates) or a switch settles (settle animates). The switch's path is not a
/// straight line between two poses, so the poses themselves are not what animates.
private struct StackCardMotion: ViewModifier, Animatable {
    var drag: Double
    var settle: Double
    let index: Int
    let role: TransferLayout.StackRole
    let count: Int
    let settling: TransferLayout.StackSettle?
    /// --c-stack-dim: cards behind the top one darken by this × depth (dark mode).
    let dim: Double

    var animatableData: AnimatablePair<Double, Double> {
        get { AnimatablePair(drag, settle) }
        set { drag = newValue.first; settle = newValue.second }
    }

    func body(content: Content) -> some View {
        let pose = settling.map { TransferLayout.settlePose($0, index: index, progress: settle) }
            ?? TransferLayout.dragPose(role, dx: drag, count: count)
        content
            // Black over the card at dim × depth, as a multiply (keeps the card's outline).
            .colorMultiply(Color(white: 1 - min(1, dim * pose.depth)))
            .scaleEffect(pose.scale)
            .rotationEffect(.degrees(pose.rotation))
            .offset(x: pose.dx)
            .opacity(pose.opacity)
            .zIndex(pose.z)
    }
}

/// The stack's swipe: a UIKit pan that only begins when the finger first moves more
/// sideways than up or down, so a vertical drag fails it at once and the list's own pan
/// scrolls. A SwiftUI DragGesture there, even as `simultaneousGesture` with a minimum
/// distance, kept the list from scrolling whenever the drag started on a stack (iOS 27
/// simulator, transfer scroll test: flings starting on a stack left the list in place).
struct HorizontalPan: UIGestureRecognizerRepresentable {
    /// Horizontal translation while the pan runs.
    let onChanged: (CGFloat) -> Void
    /// Final translation and horizontal velocity (pt/s); also on cancel.
    let onEnded: (CGFloat, CGFloat) -> Void

    func makeCoordinator(converter: CoordinateSpaceConverter) -> Coordinator { Coordinator() }

    func makeUIGestureRecognizer(context: Context) -> UIPanGestureRecognizer {
        let pan = UIPanGestureRecognizer()
        pan.delegate = context.coordinator
        return pan
    }

    func handleUIGestureRecognizerAction(_ pan: UIPanGestureRecognizer, context: Context) {
        let dx = pan.translation(in: pan.view).x
        switch pan.state {
        case .began, .changed: onChanged(dx)
        case .ended, .cancelled, .failed: onEnded(dx, pan.velocity(in: pan.view).x)
        default: break
        }
    }

    final class Coordinator: NSObject, UIGestureRecognizerDelegate {
        func gestureRecognizerShouldBegin(_ recognizer: UIGestureRecognizer) -> Bool {
            guard let pan = recognizer as? UIPanGestureRecognizer else { return true }
            let velocity = pan.velocity(in: pan.view)
            return abs(velocity.x) > abs(velocity.y)
        }
    }
}

/// One card of a stack (SPEC §2.1). Pictures: contained in 120 × 160, the rest
/// transparent, radius 9, shadow and edge on the picture itself. Files and
/// folders: a 120 × 160 card, type icon top left, extension top right, name
/// (3 lines) and size at the bottom.
struct StackCardFace: View {
    @Environment(\.oink) private var oink
    let message: TransferMessage
    let picture: ListPicture?
    let progress: Int?
    let detail: String

    var body: some View {
        let colors = StackColors(dark: oink.dark)
        if message.kind == .image {
            let size = TransferLayout.fit(aspect: picture?.aspect ?? 3.0 / 4, in: TransferLayout.stackBox)
            let shape = RoundedRectangle(cornerRadius: 9, style: .continuous)
            StackPicture(message: message, picture: picture, progress: progress)
                .frame(width: size.width, height: size.height)
                .clipShape(shape)
                .mediaEdge(shape, shown: picture != nil)
                .stackShadow(colors)
        } else {
            // The flat cards' plate and 1 pt edge; only the stack keeps the shadow (its
            // cards overlap, and only the shadow tells the layers apart).
            StackFileContent(message: message, detail: detail)
                .fileCardPlate(colors)
                .stackShadow(colors)
        }
    }
}

extension View {
    /// The picture edge (user 2026-11-12): a 0.5 pt `--c-media-edge` hairline drawn inside the
    /// picture, on top of it, following its own corners; the same on single pictures, stack
    /// cards and folder thumbnails. Placeholders (downloading, failed, cancelled) have their own
    /// fill and no edge (`shown` false).
    func mediaEdge<S: InsettableShape>(_ shape: S, shown: Bool = true) -> some View {
        modifier(MediaEdge(shape: shape, shown: shown))
    }
}

private struct MediaEdge<S: InsettableShape>: ViewModifier {
    @Environment(\.oink) private var oink
    let shape: S
    let shown: Bool

    func body(content: Content) -> some View {
        content.overlay {
            if shown { shape.strokeBorder(StackColors(dark: oink.dark).mediaEdge, lineWidth: 0.5).allowsHitTesting(false) }
        }
    }
}

/// A stack card's picture, or its placeholder before there is one.
struct StackPicture: View {
    @Environment(\.oink) private var oink
    let message: TransferMessage
    let picture: ListPicture?
    let progress: Int?
    var body: some View {
        ZStack {
            if let picture {
                ListPictureView(picture) { oink.fill }
            } else {
                oink.fill
                if let progress, progress < 100, message.state == .downloading { ProgressRing(value: progress, size: 30) }
                else { Lucide(name: "image", size: 22, color: oink.faint) }
            }
        }
    }
}

/// A file or folder card's content without its plate: 120 × 160, type block top left,
/// extension top right, name and size at the bottom. Also the morph's card face.
/// The stack shows no download state (native-rewrite.md「文件卡片」): the type icon always.
struct StackFileContent: View {
    @Environment(\.oink) private var oink
    let message: TransferMessage
    let detail: String

    var body: some View {
        let colors = StackColors(dark: oink.dark)
        let folder = message.kind == .folder
        let tint = folder ? oink.folder : oink.file
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .top, spacing: 0) {
                Lucide(name: TransferLayout.fileIcon(name: message.attachmentName, folder: folder), size: 17,
                       stroke: "1_8", color: tint)
                .frame(width: 30, height: 30)
                .background(colors.tint(tint, folder ? 0.18 : 0.16), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                Spacer(minLength: 4)
                if !folder, let ext = TransferLayout.fileExtension(message.attachmentName) {
                    Text(ext).font(.oink(9, .semibold)).kerning(0.4).foregroundStyle(colors.hint)
                        .lineLimit(1).frame(height: 12).padding(.top, -1)
                }
            }
            Spacer(minLength: 6)
            Text(message.attachmentName ?? (folder ? "文件夹" : "文件"))
                .font(.oink(12, .semibold)).foregroundStyle(colors.ink)
                .lineSpacing(1.5).lineLimit(3).truncationMode(.tail).multilineTextAlignment(.leading)
            Text(detail).font(.oink(10).monospacedDigit()).foregroundStyle(colors.hint).padding(.top, 3)
        }
        .padding(10)
        .frame(width: TransferLayout.stackBox.width, height: TransferLayout.stackBox.height, alignment: .topLeading)
    }
}

// MARK: - File cards

extension View {
    /// A file card's surface (native-rewrite.md「文件卡片」): `--c-stack-card`, radius 12,
    /// the 1 pt inner `--c-stack-card-edge`, no shadow. Received and own cards look the same.
    func fileCardPlate(_ colors: StackColors) -> some View {
        let shape = RoundedRectangle(cornerRadius: 12, style: .continuous)
        return background(colors.card, in: shape)
            .overlay(shape.strokeBorder(colors.edge, lineWidth: StackColors.edgeWidth))
    }
}

/// 36 × 36 type tile of a file card and a folder row: the type colour 16 % over the card
/// (folder 18 %), the type's icon at 20 / 1.8; grey when the content can no longer be had.
struct FileTypeTile: View {
    @Environment(\.oink) private var oink
    let name: String?
    let folder: Bool
    var grey = false

    var body: some View {
        let colors = StackColors(dark: oink.dark)
        let tint = grey ? colors.hint : folder ? oink.folder : oink.file
        Lucide(name: TransferLayout.fileIcon(name: name, folder: folder), size: 20, stroke: "1_8", color: tint)
            .frame(width: 36, height: 36)
            .background(colors.tint(tint, folder && !grey ? 0.18 : 0.16), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
            .accessibilityHidden(true)
    }
}

/// Name (at most 3 lines) and the second line: upper-case extension, 6 pt, then the size
/// or the reason that replaces it. `large`: the folder page's 14 / 1.35 name and 11 size.
struct FileCardText: View {
    @Environment(\.oink) private var oink
    let name: String
    let ext: String?
    let detail: String
    var reasonBad = false
    var large = false

    var body: some View {
        let colors = StackColors(dark: oink.dark)
        VStack(alignment: .leading, spacing: 4) {
            // Line height 1.3 (1.35 on the folder page) over the system font's own line box.
            Text(name)
                .font(.oink(large ? 14 : 12, .semibold)).lineSpacing(large ? 2.2 : 1.3)
                .foregroundStyle(colors.ink)
                .lineLimit(3).truncationMode(.tail).multilineTextAlignment(.leading)
                .fixedSize(horizontal: false, vertical: true)
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                if let ext { Text(ext).font(.oink(9, .semibold)).kerning(0.4).foregroundStyle(colors.hint).layoutPriority(1) }
                if !detail.isEmpty {
                    Text(detail).font(.oink(large ? 11 : 10).monospacedDigit())
                        .foregroundStyle(reasonBad ? oink.badInk : colors.hint).lineLimit(1)
                }
            }
            .frame(minHeight: 12)
        }
    }
}

/// The right-hand control of a received file (single card, expanded batch item, folder
/// row), 40 × 44. Download and retry are buttons when `action` is given; the other states
/// only show (a still face for the morph has no action at all).
struct FileSlotButton: View {
    @Environment(\.oink) private var oink
    let slot: TransferLayout.FileSlot
    let name: String
    var action: (() -> Void)? = nil

    var body: some View {
        if let action, slot == .download || slot == .retry {
            Button(action: action) { face }
                .buttonStyle(.plain)
                .accessibilityLabel(slot == .retry ? AppText.pick("重试下载 \(name)", "Retry download \(name)")
                                                   : AppText.pick("下载 \(name)", "Download \(name)"))
        } else {
            face
        }
    }

    private var face: some View {
        let colors = StackColors(dark: oink.dark)
        return Group {
            switch slot {
            case .download:
                Lucide(name: "download", size: 16, color: oink.text2)
                    .frame(width: 32, height: 32)
                    .background(colors.tint(colors.ink, 0.06), in: Circle())
            case .progress(let value):
                ProgressRing(value: value, trackColor: colors.ringTrack)
            case .done:
                Lucide(name: "check", size: 18, color: colors.hint).accessibilityLabel(AppText.pick("已下载", "Downloaded"))
            case .retry:
                Lucide(name: "rotate-cw", size: 15, color: oink.badInk)
                    .frame(width: 32, height: 32)
                    .background(colors.tint(oink.bad, 0.12), in: Circle())
            case .receiveFailed:
                Lucide(name: "alert-circle", size: 20, color: oink.bad).accessibilityLabel(AppText.pick("接收失败", "Not received"))
            case .sourceOffline:
                Lucide(name: "download", size: 16, color: oink.faint).opacity(0.6).accessibilityHidden(true)
            }
        }
        .frame(width: 40, height: 44)
        .contentShape(Rectangle())
    }
}

/// A file or folder message as a card's content (row layout): type tile, name and second
/// line, and a received file's control. The single message, the expanded batch item and
/// the morph's message face all draw this; the caller adds the plate. `fixedWidth`: an
/// expanded batch item is 220 wide (its open side keeps room for 收起); a single card is
/// at least 200 and grows with its name. Without `onOpen` it is a still picture.
struct FileCardContent: View {
    let message: TransferMessage
    let progress: Int?
    var fixedWidth: CGFloat? = nil
    var onOpen: (() -> Void)? = nil
    var onDownload: (() -> Void)? = nil

    private var folder: Bool { message.kind == .folder }

    var body: some View {
        let look = Self.look(message, progress: progress)
        HStack(spacing: 10) {
            if let onOpen, !look.inert {
                Button(action: onOpen) { main(look) }.buttonStyle(.plain)
            } else {
                main(look)
            }
            if let slot = look.slot {
                FileSlotButton(slot: slot, name: message.attachmentName ?? "", action: onDownload)
            }
        }
        .padding(.leading, 12).padding(.trailing, look.slot == nil ? 14 : 8).padding(.vertical, 12)
        .frame(width: fixedWidth, alignment: .leading)
        .frame(minWidth: fixedWidth == nil ? 200 : nil, minHeight: 70, alignment: .leading)
    }

    private func main(_ look: TransferLayout.FileCardLook) -> some View {
        let name = message.attachmentName ?? (folder ? AppText.pick("文件夹", "Folder") : AppText.pick("文件", "File"))
        return HStack(spacing: 12) {
            FileTypeTile(name: message.attachmentName, folder: folder, grey: look.greyTile)
            FileCardText(name: name, ext: folder ? nil : TransferLayout.fileExtension(message.attachmentName),
                         detail: look.reason.map(Self.reasonText) ?? Self.size(message),
                         reasonBad: look.reason == .downloadFailed)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
    }

    static func look(_ message: TransferMessage, progress: Int?) -> TransferLayout.FileCardLook {
        TransferLayout.fileCard(kind: message.kind, mine: message.isMine, status: message.downloadStatus,
                                delivery: message.delivery, progress: progress)
    }

    /// The size of a file, the file count of a folder (「N 个文件」).
    static func size(_ message: TransferMessage) -> String {
        if message.kind == .folder {
            if let count = message.itemCount { return AppText.pick("\(count) 个文件", "\(count) files") }
            return AppText.pick("文件夹", "Folder")
        }
        return message.byteSize.map { ByteCountFormatter.string(fromByteCount: $0, countStyle: .decimal) } ?? ""
    }

    static func reasonText(_ reason: TransferLayout.FileCardReason) -> String {
        switch reason {
        case .expired: AppText.pick("已失效", "Expired")
        case .sourceOffline: AppText.pick("暂时无法加载", "Unavailable for now")
        case .downloadFailed: AppText.pick("下载失败", "Download failed")
        }
    }
}

/// 展开 coming back after a collapse: once the morph is over it fades in (200 ms, ease
/// out) from 85 % (Android BatchStack `enter`).
struct PillEntrance: ViewModifier {
    let enter: Bool
    let hidden: Bool
    let done: () -> Void
    @State private var shown: Double

    init(enter: Bool, hidden: Bool, done: @escaping () -> Void) {
        self.enter = enter
        self.hidden = hidden
        self.done = done
        _shown = State(initialValue: enter ? 0 : 1)
    }

    func body(content: Content) -> some View {
        content
            .opacity(shown)
            .scaleEffect(0.85 + 0.15 * shown)
            .onChange(of: hidden, initial: true) { _, hidden in
                guard enter, !hidden, shown < 1 else { return }
                withAnimation(.easeOut(duration: 0.2)) { shown = 1 } completion: { done() }
            }
    }
}

/// Waits for the next display refresh(es): a layout change made before is on screen and
/// its geometry reported by then.
@MainActor
enum DisplayFrame {
    static func next(_ count: Int = 1) async {
        for _ in 0..<count {
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                FrameTick.start(continuation)
            }
        }
    }

    private final class FrameTick: NSObject {
        private var continuation: CheckedContinuation<Void, Never>?
        private var link: CADisplayLink?

        /// The display link holds the tick until it fires once.
        static func start(_ continuation: CheckedContinuation<Void, Never>) {
            let tick = FrameTick()
            tick.continuation = continuation
            let link = CADisplayLink(target: tick, selector: #selector(fire))
            tick.link = link
            link.add(to: .main, forMode: .common)
        }

        @objc private func fire() {
            link?.invalidate()
            link = nil
            continuation?.resume()
            continuation = nil
        }
    }
}
