import Foundation
import CoreGraphics

/// One row of the transfer conversation: a single message, or the
/// attachments of one share shown as a stacked batch (collapsed).
enum TransferEntry: Identifiable, Equatable {
    /// `batch`: the expanded batch the message belongs to (its rows carry the
    /// 收起 button), nil for a message outside any batch.
    case message(TransferMessage, batch: String?)
    /// A collapsed batch, oldest first.
    case stack(id: String, items: [TransferMessage])

    var id: String {
        switch self {
        case .message(let message, _): message.id
        case .stack(let id, _): "batch:\(id)"
        }
    }
    var first: TransferMessage {
        switch self {
        case .message(let message, _): message
        case .stack(_, let items): items[0]
        }
    }
    var last: TransferMessage {
        switch self {
        case .message(let message, _): message
        case .stack(_, let items): items[items.count - 1]
        }
    }
}

/// Pure layout rules of the transfer page (batches, long-text folding, the
/// composer prompt), kept apart from the views so they are unit tested.
enum TransferLayout {
    // MARK: Batches

    /// Records of one share (one received offer, one own send) carry the same
    /// operation id. Its attachments form a batch; text never does.
    static func batchKey(_ message: TransferMessage) -> String? {
        guard message.kind != .text, message.parentID == nil,
              let operation = message.operationID, !operation.isEmpty else { return nil }
        return "\(message.isMine ? "self" : message.displaySenderID)/\(operation)"
    }

    /// `ordered`: oldest first. Two or more consecutive attachments of one
    /// share become one stack, unless the batch is expanded: then each item is
    /// its own row tagged with the batch.
    static func entries(_ ordered: [TransferMessage], expanded: Set<String>) -> [TransferEntry] {
        var result: [TransferEntry] = []
        var index = 0
        while index < ordered.count {
            let message = ordered[index]
            guard let key = batchKey(message) else {
                result.append(.message(message, batch: nil))
                index += 1
                continue
            }
            var end = index + 1
            while end < ordered.count, batchKey(ordered[end]) == key { end += 1 }
            let items = Array(ordered[index..<end])
            if items.count < 2 {
                result.append(.message(message, batch: nil))
            } else if expanded.contains(key) {
                result.append(contentsOf: items.map { .message($0, batch: key) })
            } else {
                result.append(.stack(id: key, items: items))
            }
            index = end
        }
        return result
    }

    /// The send state shown under an own batch: sending while any item is,
    /// then failed if any failed (with that item's reason), else sent.
    static func delivery(_ items: [TransferMessage]) -> (TransferDeliveryState?, String?) {
        if items.contains(where: { $0.delivery == .sending }) { return (.sending, nil) }
        if let failed = items.first(where: { $0.delivery == .failed }) { return (.failed, failed.deliveryFailure) }
        if !items.isEmpty, items.allSatisfy({ $0.delivery == .sent }) { return (.sent, nil) }
        return (items.first?.delivery, nil)
    }

    /// The item of an expanded batch that carries the 收起 button: the first
    /// one (in conversation order) that is completely visible.
    static func collapseTarget(_ items: [TransferMessage], fullyVisible: Set<String>) -> String? {
        items.first { fullyVisible.contains($0.id) }?.id
    }

    /// Top card after a horizontal swipe on a collapsed stack: left (negative
    /// `dx`) shows the next card, right the previous one, circularly (after the
    /// last comes the first). It switches past 40 pt or above 0.35 pt/ms
    /// (`velocity` in pt/s); short and mostly vertical drags change nothing.
    static func swipe(top: Int, count: Int, dx: Double, dy: Double, velocity: Double = 0) -> Int {
        guard count > 1, abs(dx) > abs(dy), abs(dx) > 40 || abs(velocity) > 350 else { return top }
        let direction = (dx != 0 ? dx : velocity) < 0 ? 1 : -1
        return ((top + direction) % count + count) % count
    }

    // MARK: Page geometry (mockup transfer.css)

    /// Side margin of the conversation: 20 (native-rewrite.md 页面水平边距), 40 from a
    /// 600 pt wide list (`@container(min-width:600px)`).
    static func listMargin(width: CGFloat) -> CGFloat { width >= 600 ? 40 : 20 }

    /// The widest a message body (bubble column) may get: the row beside the 32 pt avatar
    /// and its 12 pt gap (`max-width: calc(100% - 44px)`, no cap of its own), 65 % of the
    /// row from a 600 pt wide list. `.infinity` leaves it to the row.
    static func bodyMaxWidth(width: CGFloat) -> CGFloat {
        width >= 600 ? ((width - 2 * listMargin(width: width)) * 0.65).rounded(.down) : .infinity
    }

    // MARK: Stack geometry (transfer-stack SPEC §2)

    /// Every item of a stack is reduced to this 3:4 box.
    static let stackBox = CGSize(width: 120, height: 160)
    /// An expanded batch image is contained in this box.
    static let expandedImageBox = CGSize(width: 220, height: 280)

    /// A card's place in the stack around the top card.
    enum StackRole: Equatable {
        case top, next, prev, next2, farNext, farPrev
        /// Horizontal offset, scale, rotation (degrees), opacity, z order and depth (dark-mode dimming).
        var pose: StackPose {
            switch self {
            case .top: StackPose(dx: 0, scale: 1, rotation: 0, opacity: 1, z: 6, depth: 0)
            case .next: StackPose(dx: 10, scale: 0.93, rotation: 3, opacity: 1, z: 5, depth: 1)
            case .prev: StackPose(dx: -10, scale: 0.93, rotation: -3, opacity: 1, z: 4, depth: 1)
            case .next2: StackPose(dx: 19, scale: 0.86, rotation: 5.7, opacity: 1, z: 3, depth: 1.6)
            case .farNext: StackPose(dx: 26, scale: 0.8, rotation: 7.8, opacity: 0, z: 2, depth: 2)
            case .farPrev: StackPose(dx: -18, scale: 0.86, rotation: -5.4, opacity: 0, z: 1, depth: 2)
            }
        }
    }

    struct StackPose: Equatable {
        var dx: Double, scale: Double, rotation: Double, opacity: Double, z: Double, depth: Double
        /// Towards `other` by `t`; the z order is `z` (this pose's own by default).
        func mixed(_ other: StackPose, _ t: Double, z: Double? = nil) -> StackPose {
            StackPose(dx: lerp(dx, other.dx, t), scale: lerp(scale, other.scale, t), rotation: lerp(rotation, other.rotation, t),
                      opacity: lerp(opacity, other.opacity, t), z: z ?? self.z, depth: lerp(depth, other.depth, t))
        }
    }

    static func lerp(_ a: Double, _ b: Double, _ t: Double) -> Double { a + (b - a) * t }

    // MARK: Stack motion (Android TransferBatch.kt, the reference implementation)

    /// Expand / collapse: each item morphs in 400 ms, 20 ms later per step around the ring
    /// from the top card (at most 6 steps); rows around slide in the same 400 ms.
    static let morphMs = 400.0
    static let morphStaggerMs = 20.0
    /// Swipe: a switch settles in 320 ms (linear time, standard easing on the poses); a
    /// swipe let go short of a switch springs back in 250 ms (cubic-bezier(0,0,0,1)).
    static let swipeCommitMs = 320.0
    static let swipeCancelMs = 250.0

    /// cubic-bezier(.2, 0, 0, 1), Android's EaseStandard: the swipe settle and the morph.
    static func easeStandard(_ t: Double) -> Double { cubicBezier(0.2, 0, 0, 1, clamp(t)) }

    static func clamp(_ t: Double) -> Double { min(1, max(0, t)) }

    /// y of a CSS cubic-bezier at time x (Newton steps, then bisection).
    static func cubicBezier(_ x1: Double, _ y1: Double, _ x2: Double, _ y2: Double, _ x: Double) -> Double {
        func curve(_ a: Double, _ b: Double, _ t: Double) -> Double { ((1 - 3 * b + 3 * a) * t + (3 * b - 6 * a)) * t * t + 3 * a * t }
        func slope(_ a: Double, _ b: Double, _ t: Double) -> Double { 3 * (1 - 3 * b + 3 * a) * t * t + 2 * (3 * b - 6 * a) * t + 3 * a }
        var t = x
        for _ in 0..<8 {
            let error = curve(x1, x2, t) - x
            if abs(error) < 1e-6 { return curve(y1, y2, t) }
            let d = slope(x1, x2, t)
            if abs(d) < 1e-6 { break }
            t -= error / d
        }
        var low = 0.0, high = 1.0
        t = x
        for _ in 0..<40 {
            let value = curve(x1, x2, t)
            if abs(value - x) < 1e-6 { break }
            if value < x { low = t } else { high = t }
            t = (low + high) / 2
        }
        return curve(y1, y2, t)
    }

    /// Steps around the ring from `i` to `j` among `count` items.
    static func ringDistance(_ count: Int, _ i: Int, _ j: Int) -> Int { min(abs(i - j), count - abs(i - j)) }

    /// Delay of item `index`'s morph when `anchor` is the top card (ms).
    static func morphDelay(count: Int, index: Int, anchor: Int) -> Double {
        Double(min(ringDistance(count, index, anchor), 6)) * morphStaggerMs
    }

    /// A card following the finger: the top card moves and tilts with `dx`, the card being
    /// revealed (next on a left swipe, previous on a right one; next with two cards) comes
    /// towards the top place, up to 85 % once the finger is 84 pt away.
    static func dragPose(_ role: StackRole, dx: Double, count: Int) -> StackPose {
        let base = role.pose
        if dx == 0 { return base }
        if role == .top {
            var pose = base
            pose.dx = dx
            pose.rotation = dx / stackBox.width * 11
            return pose
        }
        let incoming: StackRole = dx < 0 || count < 3 ? .next : .prev
        guard role == incoming else { return base }
        return base.mixed(StackRole.top.pose, min(1, abs(dx) / (stackBox.width * 0.7)) * 0.85)
    }

    /// A switch in progress: every drawn card goes from where the finger left it to its
    /// place around the new top card.
    struct StackSettle: Equatable {
        /// 1: the next card comes to the top (left swipe), -1: the previous one.
        var direction: Int
        var newTop: Int
        var outgoing: Int
        /// The cards drawn (around the old top) and their start and end poses.
        var roles: [Int: StackRole]
        var from: [Int: StackPose]
        var to: [Int: StackPose]
    }

    static func settle(count: Int, top: Int, dx: Double, direction: Int) -> StackSettle {
        let newTop = ((top + direction) % count + count) % count
        let now = roles(count: count, top: top), next = roles(count: count, top: newTop)
        var from: [Int: StackPose] = [:], to: [Int: StackPose] = [:]
        for (index, role) in now {
            from[index] = dragPose(role, dx: dx, count: count)
            if let end = next[index] { to[index] = end.pose }
        }
        return StackSettle(direction: direction, newTop: newTop, outgoing: top, roles: now, from: from, to: to)
    }

    /// Pose of card `index` at `progress` (linear time 0…1) of a switch. The others ease
    /// to their new places; the card leaving the top flies out in the swipe direction above
    /// everything until 46 %, drops below the others by 48 % and tucks into its new place.
    static func settlePose(_ settle: StackSettle, index: Int, progress: Double) -> StackPose {
        guard let from = settle.from[index] else { return settle.roles[index]?.pose ?? StackRole.farPrev.pose }
        guard let to = settle.to[index] else { return from }
        let p = easeStandard(progress)
        if index != settle.outgoing { return from.mixed(to, p, z: lerp(from.z, to.z, p)) }
        let dir = Double(settle.direction), width = stackBox.width
        let out = StackPose(dx: -dir * width * 0.78, scale: 0.97, rotation: -dir * 12, opacity: 1, z: 9, depth: 0)
        let tuck = StackPose(dx: -dir * width * 0.74, scale: 0.95, rotation: -dir * 11, opacity: 1, z: to.z, depth: 0)
        let fade = out.mixed(to, clamp((p - 0.46) / 0.54))
        if p < 0.46 {
            var start = from
            start.z = 9
            return start.mixed(out, p / 0.46, z: 9)
        }
        var pose = p < 0.5 ? out.mixed(tuck, (p - 0.46) / 0.04, z: p < 0.48 ? 9 : to.z) : tuck.mixed(to, (p - 0.5) / 0.5, z: to.z)
        pose.opacity = fade.opacity
        pose.depth = fade.depth
        return pose
    }

    // MARK: Expand / collapse morph (Android BatchMorph)

    /// One item's stand-in while it morphs: an untransformed rect in the list's viewport and
    /// the card's pose around its centre. `card`: the stack card look (radius, plate, shadow),
    /// otherwise the message look.
    struct MorphGeo: Equatable {
        var rect: CGRect
        var dx = 0.0, scale = 1.0, rotation = 0.0, opacity = 1.0, depth = 0.0
        var card: Bool
    }

    /// The card of an item in the stack whose 120 × 160 box is at `box` (its picture
    /// contained in the box, centred).
    static func cardGeo(box: CGPoint, size: CGSize, pose: StackPose) -> MorphGeo {
        MorphGeo(rect: CGRect(x: box.x + (stackBox.width - size.width) / 2, y: box.y + (stackBox.height - size.height) / 2,
                              width: size.width, height: size.height),
                 dx: pose.dx, scale: pose.scale, rotation: pose.rotation, opacity: pose.opacity, depth: pose.depth, card: true)
    }

    /// An item outside the visible cards: shrunk into the stack's middle, invisible.
    static func hiddenGeo(box: CGPoint, size: CGSize, card: Bool) -> MorphGeo {
        var geo = cardGeo(box: box, size: size, pose: StackPose(dx: 0, scale: 0.9, rotation: 0, opacity: 0, z: 0, depth: 0))
        geo.card = card
        return geo
    }

    /// Opacity of a file item's card content and of its message content at eased progress
    /// `e`: when expanding the card fades out in the first 45 % and the message comes in
    /// from 30 %; collapsing, the message fades out in the first 45 % and the card comes in
    /// after 55 %. An item drawn only as a card or only as a message keeps that one.
    static func morphFaces(e: Double, hasCard: Bool, hasBubble: Bool, toBubble: Bool) -> (card: Double, bubble: Double) {
        let card: Double = !hasCard ? 0 : !hasBubble ? 1 : toBubble ? 1 - clamp(e / 0.45) : clamp((e - 0.55) / 0.45)
        let bubble: Double = !hasBubble ? 0 : !hasCard ? 1 : toBubble ? clamp((e - 0.3) / 0.7) : 1 - clamp(e / 0.45)
        return (card, bubble)
    }

    /// The drawn cards around `top`, circularly: n = 2 top + next, n = 3 adds prev,
    /// n ≥ 4 adds next2; the invisible far layers give every card of a swipe a
    /// start and end place. Each index appears once (earlier roles win).
    static func roles(count: Int, top: Int) -> [Int: StackRole] {
        guard count > 0 else { return [:] }
        var out: [Int: StackRole] = [:]
        let wanted: [(StackRole, Int)] = [(.top, 0), (.next, 1), (.prev, -1), (.next2, 2), (.farNext, 3), (.farPrev, -2)]
        for (role, offset) in wanted {
            if role == .prev && count < 3 { continue }
            if role == .next2 && count < 4 { continue }
            let index = ((top + offset) % count + count) % count
            if out[index] == nil { out[index] = role }
        }
        return out
    }

    /// `aspect` (width / height) contained in `box`.
    static func fit(aspect: Double, in box: CGSize) -> CGSize {
        guard aspect > 0 else { return box }
        return aspect >= box.width / box.height
            ? CGSize(width: box.width, height: (box.width / aspect).rounded())
            : CGSize(width: (box.height * aspect).rounded(), height: box.height)
    }

    /// Scroll change that keeps the batch in place when it expands (SPEC §4.2):
    /// the top item's message lands where the stack box was. `heights` are the
    /// expanded items' heights (10 pt apart); the box sits 4 pt into its row.
    /// Collapsing onto item `top` is the same change, negated (§4.3).
    static func expandShift(heights: [Double], top: Int) -> Double {
        heights.prefix(top).reduce(0) { $0 + $1 + 10 } - 4
    }

    /// How much taller a batch gets when it expands: its items 10 pt apart
    /// instead of the 160 pt stack box with 4 pt above and below.
    static func batchGrowth(heights: [Double]) -> Double {
        heights.reduce(0, +) + 10 * Double(max(0, heights.count - 1)) - 168
    }

    /// Lucide icon of a file card by extension (SPEC §2.1).
    static func fileIcon(name: String?, folder: Bool) -> String {
        if folder { return "folder" }
        switch fileExtension(name)?.lowercased() {
        case "pdf", "doc", "docx", "txt", "md", "rtf", "pages", "odt": return "file-text"
        case "xls", "xlsx", "csv", "numbers", "ods": return "file-spreadsheet"
        case "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz": return "file-archive"
        default: return "file"
        }
    }

    /// Upper-case extension shown on a file card, nil without one.
    static func fileExtension(_ name: String?) -> String? {
        guard let name, let dot = name.lastIndex(of: "."), dot != name.startIndex else { return nil }
        let ext = name[name.index(after: dot)...]
        return ext.isEmpty || ext.count > 5 ? nil : ext.uppercased()
    }

    // MARK: File cards (native-rewrite.md「文件卡片」, file-bubble SPEC)

    /// The right-hand control of a received file: single cards, expanded batch items and
    /// folder rows share it (one view, TransferParts `FileSlotButton`).
    enum FileSlot: Equatable {
        /// Not downloaded: 32 round chip with the download icon.
        case download
        /// Downloading, or a phone-to-phone receive arriving: the ring with its number.
        case progress(Int)
        /// Downloaded: the check.
        case done
        /// The download failed and can be tried again: red retry chip.
        case retry
        /// A phone-to-phone receive failed: red alert, never retried.
        case receiveFailed
        /// The source device is offline: the download icon without chip, not tappable.
        case sourceOffline
    }

    /// What replaces the size on a card's second line.
    enum FileCardReason: Equatable {
        /// 「已失效」: no holder has the body.
        case expired
        /// 「暂时无法加载」: all holders are offline.
        case sourceOffline
        /// 「下载失败」 in red (with the retry chip).
        case downloadFailed
    }

    struct FileCardLook: Equatable {
        var slot: FileSlot?
        var reason: FileCardReason?
        /// The type tile turns grey: cancelled, a phone-to-phone receive that failed, content gone.
        var greyTile = false
        /// Tapping the card does nothing (content gone).
        var inert = false
    }

    /// A file or folder card's control, second-line reason and tile colour from the core's
    /// download status (file-bubble SPEC §1.3, §4.1). `progress`: 0–100 of a download or
    /// receive in flight. Own records have no control; a send the receiving phone cancelled
    /// greys the tile, a failed send does not. A folder has no control, only the looks.
    static func fileCard(kind: TransferMessageKind, mine: Bool, status: TransferDownloadStatus?,
                         delivery: TransferDeliveryState?, progress: Int?) -> FileCardLook {
        if mine { return FileCardLook(greyTile: delivery == .cancelled) }
        let file = kind != .folder
        switch status {
        case .downloaded: return FileCardLook(slot: file ? .done : nil)
        case .downloading: return FileCardLook(slot: file ? .progress(min(99, max(0, progress ?? 0))) : nil)
        case .receiveFailed: return FileCardLook(slot: file ? .receiveFailed : nil, greyTile: true)
        case .cancelled: return FileCardLook(greyTile: true)
        case .expired: return FileCardLook(reason: .expired, greyTile: true, inert: true)
        case .sourceOffline: return FileCardLook(slot: file ? .sourceOffline : nil, reason: .sourceOffline)
        case .downloadFailed: return file ? FileCardLook(slot: .retry, reason: .downloadFailed) : FileCardLook()
        case .notDownloaded, nil: return FileCardLook(slot: file ? .download : nil)
        }
    }

    // MARK: Pictures (native-rewrite.md「图片尺寸」)

    /// Received and own pictures are both 220 wide (user 2026-11-11).
    static let imageWidth: CGFloat = 220
    /// The shape before the proportions are known (260 : 146, 220 × 124).
    static let imagePlaceholderAspect = 260.0 / 146

    /// Width / height of a picture message: the core's `image_width/height` (an own
    /// picture's original as sent, a received one's thumbnail) held to 0.75–2, the
    /// 260 : 146 placeholder while unknown. A received picture keeps that shape when its
    /// original arrives (no jump).
    static func imageAspect(_ known: Double?) -> Double {
        guard let known, known.isFinite, known > 0 else { return imagePlaceholderAspect }
        return min(max(known, 0.75), 2)
    }

    static func imageSize(aspect: Double) -> CGSize {
        CGSize(width: imageWidth, height: (imageWidth / aspect).rounded())
    }

    // MARK: Send state line (native-rewrite.md「发送状态行「已发送」」)

    /// The rows of one message for the send state: a batch (collapsed or expanded) is one.
    static func sendGroup(_ entry: TransferEntry) -> String {
        switch entry {
        case .message(let message, let batch): batch.map { "batch:\($0)" } ?? message.id
        case .stack(let key, _): "batch:\(key)"
        }
    }

    /// Progress and trouble keep their state; success captions belong to every direct bubble.
    static func showsDeliveryLine(state: TransferDeliveryState?, direct: Bool) -> Bool {
        guard let state else { return false }
        return state == .sent ? direct : true
    }

    // MARK: Folder page

    /// Picture files of a folder listing (their row shows a thumbnail when there is one).
    static func isPictureName(_ name: String) -> Bool {
        let ext = (name as NSString).pathExtension.lowercased()
        return ["jpg", "jpeg", "png", "heic", "heif", "gif", "webp", "bmp", "tif", "tiff", "avif"].contains(ext)
    }

    // MARK: Long text

    /// Lines a long text shows before 查看更多.
    static let foldedLines = 10
    /// Lines each 查看更多 adds.
    static let moreLines = 20
    /// Alpha stops of the fold mask (SPEC §1.2), as (distance above the bottom in
    /// lines, alpha): lines 1–7 opaque, then 1 → 0.66 → 0.30 → 0 at the bottom.
    static let fadeStops: [(lines: Double, alpha: Double)] = [(3, 1), (2, 0.66), (1, 0.30), (0, 0)]

    // MARK: Composer

    /// The composer by the group's other devices, online or offline (native-rewrite.md
    /// 「输入框和设备选择按组内其它设备变化」, user 2026-11-11).
    enum Composer: Equatable {
        /// Exactly one other device: no picker, everything goes to it; disabled while it is offline.
        case single(SendTarget)
        /// None or several: the picker over the online devices. `target` is the chosen
        /// device, or the one that took over; nil when no other device is online.
        case choose(target: SendTarget?)

        var target: SendTarget? {
            switch self {
            case .single(let device): device
            case .choose(let target): target
            }
        }
        /// Text field and attachments usable.
        var enabled: Bool {
            switch self {
            case .single(let device): device.online
            case .choose(let target): target != nil
            }
        }
        var showsPicker: Bool { if case .choose = self { true } else { false } }
    }

    /// `chosen`: the last device picked. An offline (or removed) choice gives way to
    /// another online device: computers first, then mobile devices, in the core's
    /// connection order (review 2026-11-11).
    static func composer(_ targets: SendTargets, chosen: String?) -> Composer {
        if targets.otherDevices == 1, let only = targets.onlyDevice { return .single(only) }
        if let chosen, let device = targets.online.first(where: { $0.id == chosen }) { return .choose(target: device) }
        return .choose(target: targets.online.first { $0.id == targets.activeDevice } ?? fallbackTarget(targets.online))
    }

    /// First online device to switch to: computers first, then mobile devices, in order.
    static func fallbackTarget(_ online: [SendTarget]) -> SendTarget? {
        online.first { $0.online }
    }

    /// Placeholder of the transfer input. A computer passes what is sent to the whole
    /// group (paste anywhere); files sent to a phone go to that phone only.
    static func composerPrompt(_ composer: Composer, chinese: Bool) -> String {
        switch composer {
        case .single(let device):
            if !device.online { return chinese ? "\(device.name) 不在线" : "\(device.name) is offline" }
            return chinese ? "发送给 \(device.name)" : "Send to \(device.name)"
        case .choose(nil):
            return chinese ? "没有其它设备在线" : "No other devices online"
        case .choose(let target?):
            if !target.isComputer { return chinese ? "发送给 \(target.name)" : "Send to \(target.name)" }
            return chinese ? "发送给 \(target.name)，可在所有设备粘贴" : "Send to \(target.name), paste on any device"
        }
    }

    /// The light hint when the chosen device went offline and another took over.
    static func switchNotice(from: String, chinese: Bool) -> String {
        chinese ? "\(from) 已离线，已切回当前活跃" : "\(from) is offline. Switched back to Currently active."
    }

    // MARK: Phone-to-phone lines

    /// The line under an own message: its send state, naming the phone of a
    /// phone-to-phone send (nil: nothing to show).
    static func deliveryLine(_ state: TransferDeliveryState?, peer: String?, progress: Int?, failure: String?,
                             chinese: Bool) -> String? {
        switch state {
        case .sending:
            let percent = progress.flatMap { $0 > 0 && $0 < 100 ? $0 : nil }
            if let peer {
                let base = chinese ? "正在发送给 \(peer)" : "Sending to \(peer)"
                return percent.map { "\(base) · \($0)%" } ?? (chinese ? "\(base)…" : "\(base)…")
            }
            return percent.map { chinese ? "发送进度 \($0)%" : "Sending \($0)%" } ?? (chinese ? "正在发送…" : "Sending…")
        case .sent:
            guard let peer else { return nil }
            return chinese ? "已发送给 \(peer)" : "Sent to \(peer)"
        case .failed:
            return failure ?? (chinese ? "发送失败" : "Send failed")
        case .cancelled:
            return chinese ? "已取消" : "Cancelled"
        case nil:
            return nil
        }
    }

    /// The line under a phone-to-phone record received straight from another phone;
    /// `failure` is the core's reason as the user reads it (「对方已断开，接收失败」…).
    static func receiveLine(_ state: AttachmentState?, failure: String?, chinese: Bool) -> String? {
        switch state {
        case .failed: failure ?? (chinese ? "接收失败" : "Receive failed")
        case .cancelled: chinese ? "已取消" : "Cancelled"
        default: nil
        }
    }

    /// What the long-press menu offers to stop a transfer in flight (review 2026-11-11):
    /// an own send (to a computer or a phone) only「取消发送」; a phone-to-phone receive
    /// 「取消接收」before 删除.
    enum InFlight: Equatable { case sending, receiving }

    static func inFlight(_ message: TransferMessage) -> InFlight? {
        if message.isMine { return message.delivery == .sending ? .sending : nil }
        return message.isDirect && message.state == .downloading ? .receiving : nil
    }

    /// Whether the long-press menu offers 复制: a single message or an item of an expanded
    /// batch with content to paste (PasteboardCopy.canCopy); a collapsed stack never, not
    /// even when its top card could be copied (user 2026-11-12: 未展开的聚合长按不能复制).
    static func offersCopy(_ message: TransferMessage, collapsedStack: Bool) -> Bool {
        !collapsedStack && PasteboardCopy.canCopy(message)
    }

    enum MenuItem: Hashable { case copy, copyText, select, delete }

    /// The long-press menu of a message that is not in flight (TransferLayout.inFlight) and not
    /// being selected, top to bottom. A text with rich text (user 2026-11-12): 复制 first (rich
    /// text and plain text), then 复制文本 (plain text only); plain text and HTML that kept no
    /// formatting have no 复制文本. 选择 for texts (a secret only once shown), 删除 last.
    static func menu(_ message: TransferMessage, rich: Bool, collapsedStack: Bool, revealed: Bool) -> [MenuItem] {
        var items: [MenuItem] = []
        if offersCopy(message, collapsedStack: collapsedStack) {
            items.append(.copy)
            if rich && message.kind == .text { items.append(.copyText) }
        }
        if message.kind == .text, !collapsedStack, !message.sensitive || (revealed && message.text != nil) { items.append(.select) }
        items.append(.delete)
        return items
    }
}
