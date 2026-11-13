import CoreGraphics
import Foundation

// Touchpad gestures of the control page (docs control-page.md §4.1, §4.2), as a pure state
// machine over touch samples in pad points. The view feeds UIKit touches in and turns the
// outputs into control calls (pointer / scroll / buttons) and the edge-lock visuals.
// The phone is a Mac trackpad source (user 2026-11-12). Two or more moving fingers are the core's
// (user 2026-11-12: iOS and Android identical, as a Mac trackpad): every touch frame goes to its
// shared recognizer (`MobileControl.recognize`), which decides two-finger scroll, pinch and
// rotate and the Dock gestures (three / four finger swipes, four / five finger pinch) the way the
// Mac's trackpad driver does, and says whether it took the touch (`CoreClaim`, passed in with
// each event). This recognizer keeps one finger (pointer, taps, tap-to-drag, edge lock), the
// two-finger tap (right click, smart zoom) and the "单指划动 = 滚动" swap (one finger scrolls, two
// move the pointer), and tells the core per event whether the touch's fingers are free for it
// (`coreMayRecognize`). Three-finger drag is not done (off by default on a Mac).
//
// One finger moves the pointer from its first movement (only ~0.15 mm of noise is held back);
// a tap is a touch that stays within the 1.5 mm slop for less than `tapTime`, and the little it
// moved is sent too, as on a Mac trackpad.
//
// Tap to drag (always on, 2026-11-10): a one-finger tap presses the left button at once and
// holds the release back for `dragWindow`. A finger that lands within that window and moves
// drags with the button still down, and releases it when it lifts; a second tap in the window
// completes the first click and clicks again (double click); otherwise the release goes out
// when the window ends — like a Mac trackpad with dragging on. Edge lock exists only while
// dragging: lifting with full progress locks the drag (user 2026-11-12, like macOS drag lock):
// the button stays down through any number of relay fingers — each lift locks again, wherever it
// is — until 释放.

/// Edge lock geometry (§4.2): a one-finger drag near the pad's edge.
struct EdgeLock: Equatable {
    enum Kind: Equatable { case sideV, sideH, corner }
    static let start: CGFloat = 64      // glow appears this close to an edge
    static let full: CGFloat = 16       // progress is full this close
    static let badgeGap: CGFloat = 46   // lock badge sits this far inside the finger

    var kind: Kind
    /// Glow centre: the edge point nearest the finger, or the corner.
    var center: CGPoint
    /// Lock badge centre, on the line from `center` through the finger, past the finger.
    var badge: CGPoint
    /// 0 at `start`, 1 at `full` and closer.
    var progress: CGFloat
    var isFull: Bool { progress >= 1 }

    static func at(_ p: CGPoint, in size: CGSize) -> EdgeLock? {
        let dx = min(p.x, size.width - p.x), dy = min(p.y, size.height - p.y), d = min(dx, dy)
        guard d < start else { return nil }
        let progress = min(1, max(0, (start - d) / (start - full)))
        let sx: CGFloat = p.x < size.width / 2 ? 0 : size.width, sy: CGFloat = p.y < size.height / 2 ? 0 : size.height
        let ix: CGFloat = sx == 0 ? 1 : -1, iy: CGFloat = sy == 0 ? 1 : -1 // inward
        let inset = d + badgeGap
        if dx < start && dy < start {
            return EdgeLock(kind: .corner, center: CGPoint(x: sx, y: sy),
                            badge: CGPoint(x: sx + ix * inset * 0.71, y: sy + iy * inset * 0.71), progress: progress)
        }
        return dx < dy
            ? EdgeLock(kind: .sideV, center: CGPoint(x: sx, y: p.y), badge: CGPoint(x: sx + ix * inset, y: p.y), progress: progress)
            : EdgeLock(kind: .sideH, center: CGPoint(x: p.x, y: sy), badge: CGPoint(x: p.x, y: sy + iy * inset), progress: progress)
    }
}

enum ClickKind: Equatable { case single, double, right }
enum TrackpadButton: Equatable { case left, right }

/// The phase of a touchpad scroll of the fingers; the core adds the momentum after Ended.
enum PadScrollPhase: Equatable { case began, changed, ended, cancelled }

/// A gesture this recognizer decides itself (`ControlGesture`); the multi-finger gestures are the
/// core's.
enum PadGesture: Equatable {
    /// Two-finger double tap.
    case smartMagnify
}

/// What the core's shared multi-finger recognizer does with the current touch
/// (`ControlTouchClaim`).
enum CoreClaim: Equatable {
    /// Nothing (yet): the touch is this recognizer's.
    case unclaimed
    /// A two-finger scroll: the press highlight follows the fingers.
    case scroll
    /// A gesture, or a touch whose gesture is over: nothing more happens here for this touch (no
    /// tap, its remaining fingers move no pointer).
    case gesture
}

enum TrackpadOutput: Equatable {
    /// One-finger (or swapped two-finger) motion, pad points, one per touch event: `at` is the
    /// event's touch time and `since` the time the motion is measured from (the previous motion
    /// of this touch, or the touch-down / finger change for its first); the core takes the finger
    /// speed for the pointer acceleration from them.
    case move(CGVector, at: TimeInterval, since: TimeInterval)
    /// The swapped one-finger scroll, pad points, finger direction, timed like `move` (two-finger
    /// scrolls are the core's).
    case scroll(CGVector, PadScrollPhase, at: TimeInterval, since: TimeInterval)
    case gesture(PadGesture)
    /// A button goes down or up on the focus computer.
    case button(TrackpadButton, down: Bool)
    /// Click feedback (ripple + screen reader) where the tap landed.
    case ripple(ClickKind, at: CGPoint)
    /// Immediate feedback for a completed tap, independent of delayed right-click disambiguation.
    case tapFeedback
    /// A tap left the left button down: call `tapWindowEnded(at:)` after `dragWindow`.
    case startTapWindow
    /// A two-finger tap is held back: call `rightTapWindowEnded(at:)` after `twoFingerDoubleTapTime`.
    case startRightTapWindow
    /// Press highlight position, nil when the last finger lifts.
    case press(CGPoint?)
    /// Edge glow and lock badge; nil hides them.
    case edge(EdgeLock?)
    /// Lock progress just became full (medium impact haptic, once per reach).
    case edgeFull
    /// The dragging finger lifted with the drag locked (full progress the first time, anywhere
    /// after a relay): it stays as the locked finger.
    case locked(CGPoint)
    /// The locked finger lifted: a new finger started moving (relay) or 释放 was tapped.
    case unlocked
}

struct TrackpadRecognizer {
    static let tapTime: TimeInterval = 0.2          // one-finger tap (§4.1)
    static let twoFingerTapTime: TimeInterval = 0.3 // first finger down → last finger up
    static let dragWindow: TimeInterval = 0.3       // tap, then press again within this to drag
    static let relayMove: CGFloat = 6               // §4.2 the new finger "starts moving"
    static let doubleClickFeedback: TimeInterval = 0.5 // ripple only (macOS default); the target decides
    /// A two-finger tap waits this long for a second one (smart zoom) before it right-clicks
    /// (user 2026-11-12: a Mac trackpad decides with force feedback; we delay instead).
    static let twoFingerDoubleTapTime: TimeInterval = 0.3
    static let maxFingers = 5                       // a trackpad's contacts; more ignores the touch
    /// The touchpad counts as idle this long after the last finger lifted (and the drag lock let go).
    /// Defers keyboard changes from the computer's text focus (PCTextFocusGate).
    /// The floating top bar stays visible during pad use; this delay is only for text focus.
    static let touchpadIdleDelay: TimeInterval = 1.2

    /// Movement threshold in pad points (≈ 1.5 mm): a tap stays within it, and modes (drag after a
    /// tap, the swapped scroll and two-finger pointer) are decided once the fingers pass it.
    var slop: CGFloat
    /// Finger noise held back before a lone finger moves the pointer (≈ 0.15 mm).
    var noise: CGFloat { slop / 10 }
    /// 调节 › 单指划动 = 滚动: one finger scrolls, two fingers move the pointer, no drag or edge lock.
    var oneFingerScroll = false
    var padSize: CGSize = .zero

    /// `scroll`: the swapped one-finger scroll. `core`: the core took the touch (`scroll`: its
    /// two-finger scroll, the press follows the fingers).
    private enum Mode: Equatable { case undecided, pointer, drag, scroll, core(scroll: Bool), ignored }
    private struct Session {
        var start: TimeInterval
        var startPoint: CGPoint
        var maxFingers = 1
        var moved = false
        var mode = Mode.undecided
        /// Centroid the next delta is measured from (reset when the finger count changes).
        var anchor: CGPoint
        var last: CGPoint
        /// Touch time of `last`.
        var lastAt: TimeInterval
        /// The scroll's Began went out.
        var scrolling = false
        var edge: EdgeLock?
        /// Landed while a tap's left button was still down (drag or second click).
        var afterTap: Bool
        /// Landed while a dragging finger was locked: it can take the drag over.
        var relay: Bool
    }

    private var touches: [Int: CGPoint] = [:]
    private var session: Session?
    /// The left button is down after a tap, waiting for the drag window to end.
    private var tapPending: TimeInterval?
    /// Where the locked dragging finger stays (§4.2) while no finger drags; the button is down.
    private(set) var lockedAt: CGPoint?
    /// The drag is locked: from the first edge lock until 释放, lifting the dragging finger never
    /// releases the button, it locks again.
    private(set) var dragLocked = false
    /// Last plain click while one finger scrolls (double-click ripple only).
    private var lastClick: TimeInterval?
    /// A two-finger tap held back (when it ended, where it landed): a second two-finger tap starting
    /// within `twoFingerDoubleTapTime` makes it smart zoom, otherwise it right-clicks then.
    private var rightPending: (t: TimeInterval, at: CGPoint)?

    init(slop: CGFloat) { self.slop = slop }

    /// The held-back two-finger tap right-clicks now (its window ended, or the next touch is
    /// something else, which must not come before it).
    private mutating func fireRight() -> [TrackpadOutput] {
        guard let pending = rightPending else { return [] }
        rightPending = nil
        return [.button(.right, down: true), .button(.right, down: false), .ripple(.right, at: pending.at)]
    }

    /// The window after a two-finger tap ended: it right-clicks, unless a touch that may be the
    /// second two-finger tap is still on the pad (its lift decides).
    mutating func rightTapWindowEnded(at t: TimeInterval) -> [TrackpadOutput] {
        guard let pending = rightPending, t - pending.t >= Self.twoFingerDoubleTapTime - 0.001 else { return [] }
        if let s = session, s.mode == .undecided, s.maxFingers <= 2, s.start - pending.t <= Self.twoFingerDoubleTapTime {
            return []
        }
        return fireRight()
    }

    /// The left button is down on the focus computer because of this recognizer.
    var leftDown: Bool { tapPending != nil || lockedAt != nil || dragLocked || session?.mode == .drag }

    private var centroid: CGPoint { Self.centroid(touches) }

    private static func centroid(_ points: [Int: CGPoint]) -> CGPoint {
        guard !points.isEmpty else { return .zero }
        let n = CGFloat(points.count)
        return CGPoint(x: points.values.map(\.x).reduce(0, +) / n, y: points.values.map(\.y).reduce(0, +) / n)
    }

    /// Whether the core's recognizer may take this touch's fingers on the next touch event, with
    /// `fingers` fingers on the pad after it (asked before the event is handed in). Not while this
    /// recognizer uses them: a drag, the swapped one-finger scroll, two fingers moving the pointer
    /// (单指划动 = 滚动), an ignored touch.
    func coreMayRecognize(fingers: Int) -> Bool {
        if case .core = session?.mode { return true }
        if oneFingerScroll && fingers == 2 { return false }
        guard let s = session else { return true }
        switch s.mode {
        case .undecided, .pointer, .core: return true
        case .drag, .scroll, .ignored: return false
        }
    }

    /// The core took the touch (`claim` is not `.unclaimed`): from now on it is the core's; a
    /// held-back right click goes first (its outputs follow this recognizer's for the event).
    private mutating func adopt(_ claim: CoreClaim, _ s: inout Session) -> [TrackpadOutput] {
        guard claim != .unclaimed else { return [] }
        switch s.mode {
        case .core:
            s.mode = .core(scroll: claim == .scroll)
            return []
        case .undecided, .pointer:
            var out = fireRight()
            if s.afterTap { s.afterTap = false; out += finishTap() }
            if s.edge != nil { s.edge = nil; out.append(.edge(nil)) }
            s.moved = true
            s.mode = .core(scroll: claim == .scroll)
            return out
        case .drag, .scroll, .ignored:
            // Never offered to the core (`coreMayRecognize`).
            return []
        }
    }

    /// Completes a pending tap click.
    private mutating func finishTap() -> [TrackpadOutput] {
        guard tapPending != nil else { return [] }
        tapPending = nil
        return [.button(.left, down: false)]
    }

    /// Ends the swapped one-finger scroll when the touch ends (the core may glide on) or is
    /// cancelled. The core ends what it runs itself.
    private func endMode(_ s: Session, at t: TimeInterval, cancel: Bool) -> [TrackpadOutput] {
        switch s.mode {
        case .scroll:
            return [.scroll(.zero, cancel ? .cancelled : .ended, at: t, since: s.lastAt)]
        case .undecided, .pointer, .drag, .core, .ignored:
            return []
        }
    }

    /// A finger landed. `core`: the core's claim after this event.
    mutating func began(_ id: Int, at p: CGPoint, time t: TimeInterval, core: CoreClaim = .unclaimed) -> [TrackpadOutput] {
        touches[id] = p
        guard var s = session else {
            let afterTap = tapPending.map { t - $0 <= Self.dragWindow } ?? false
            var out: [TrackpadOutput] = []
            if tapPending != nil && !afterTap { out += finishTap() }
            session = Session(start: t, startPoint: p, anchor: p, last: p, lastAt: t, afterTap: afterTap,
                              relay: lockedAt != nil)
            return out + [.press(p)]
        }
        var out: [TrackpadOutput] = []
        s.maxFingers = max(s.maxFingers, touches.count)
        if s.edge != nil { s.edge = nil; out.append(.edge(nil)) }
        // A second finger: not a drag or a second click any more.
        if s.afterTap { s.afterTap = false; out += finishTap() }
        switch s.mode {
        case .core:
            // The core's: more fingers are its business (it cancels a running gesture).
            break
        case _ where touches.count > Self.maxFingers:
            out += endMode(s, at: t, cancel: true)
            if s.mode == .drag && !dragLocked { out.append(.button(.left, down: false)) }
            s.mode = .ignored
        case .pointer:
            // A finger joins a pointer move: decide again from here (the core may take it).
            s.mode = .undecided
        case .scroll:
            // The swapped one-finger scroll keeps scrolling with the fingers' centroid (from the
            // new centroid, no jump) until the touch ends.
            break
        case .drag:
            // A second finger ends a plain drag; a locked drag keeps its button and locks again
            // when the fingers lift. Extra fingers move nothing.
            s.mode = .ignored
            if !dragLocked { out.append(.button(.left, down: false)) }
        case .undecided, .ignored:
            break
        }
        out += adopt(core, &s)
        s.anchor = centroid
        s.last = s.anchor
        s.lastAt = t
        session = s
        return out
    }

    mutating func moved(_ id: Int, to p: CGPoint, time t: TimeInterval, core: CoreClaim = .unclaimed) -> [TrackpadOutput] {
        moved([id: p], time: t, core: core)
    }

    /// The pad moved under the fingers by `d` (a layout change during a touch: the keyboard, a
    /// rotation, the tab bar): the fingers did not move, so every remembered position moves into
    /// the pad's new coordinates and the next event measures only real finger motion.
    mutating func shift(by d: CGVector) {
        guard var s = session, d != .zero else { return }
        let move = { (p: CGPoint) in CGPoint(x: p.x + d.dx, y: p.y + d.dy) }
        touches = touches.mapValues(move)
        s.startPoint = move(s.startPoint)
        s.anchor = move(s.anchor)
        s.last = move(s.last)
        session = s
    }

    /// One touch event: every finger that moved in it, at the event's time. The fingers' centroid
    /// moves once per event, so two fingers moving the pointer make one motion, not two a few
    /// microseconds apart (which read as a huge finger speed). `core`: the core's claim after this
    /// event.
    mutating func moved(_ points: [Int: CGPoint], time t: TimeInterval, core: CoreClaim = .unclaimed) -> [TrackpadOutput] {
        guard var s = session else { return [] }
        var known = false
        for (id, p) in points where touches[id] != nil {
            touches[id] = p
            known = true
        }
        guard known else { return [] }
        // With one finger on the pad the centroid is that finger.
        let c = centroid
        var out = adopt(core, &s)
        if s.relay && s.mode == .undecided && touches.count == 1
            && hypot(c.x - s.startPoint.x, c.y - s.startPoint.y) > Self.relayMove {
            // Relay: the new finger takes the drag over; the locked finger lifts (button stays down).
            s.relay = false
            lockedAt = nil
            s.moved = true
            s.mode = .drag
            out.append(.unlocked)
        }
        if s.mode == .undecided && touches.count >= 2 && !(oneFingerScroll && touches.count == 2) {
            // Two or more fingers are the core's to decide (scroll, pinch, rotate, Dock gestures);
            // it takes the touch through `core` once they do something. Until then: nothing.
            session = s
            return out
        }
        if s.mode == .undecided {
            let travel = hypot(c.x - s.anchor.x, c.y - s.anchor.y)
            guard travel > slop else {
                // A lone finger that may still be a tap moves the pointer from its first movement,
                // holding back only `noise` (like a Mac trackpad); the tap is still a touch that
                // stays within the slop. A finger after a tap (drag or second click), a relay
                // finger, a touch that had more fingers and one-finger scrolling still wait for
                // the slop to decide.
                if touches.count == 1 && s.maxFingers == 1 && !oneFingerScroll && !s.afterTap && !s.relay
                    && hypot(c.x - s.last.x, c.y - s.last.y) > noise {
                    // A held-back right click goes first, where the pointer still is.
                    out += fireRight()
                    out.append(.move(CGVector(dx: c.x - s.last.x, dy: c.y - s.last.y), at: t, since: s.lastAt))
                    s.last = c
                    s.lastAt = t
                }
                session = s
                return out
            }
            s.moved = true
            // This touch is not a second two-finger tap: a held-back right click goes first.
            out += fireRight()
            switch touches.count {
            case 1 where s.afterTap && !oneFingerScroll:
                // Tap, then press again and move: drag with the tap's button still down.
                s.mode = .drag
                s.afterTap = false
                tapPending = nil
            case 1: s.mode = oneFingerScroll ? .scroll : .pointer
            // Two fingers move the pointer (单指划动 = 滚动).
            default: s.mode = .pointer
            }
            if s.afterTap { s.afterTap = false; out += finishTap() }
        }
        let d = CGVector(dx: c.x - s.last.x, dy: c.y - s.last.y)
        let since = s.lastAt
        s.last = c
        s.lastAt = t
        switch s.mode {
        case .pointer:
            out.append(.move(d, at: t, since: since))
            out.append(.press(c))
        case .drag:
            out.append(.move(d, at: t, since: since))
            out.append(.press(c))
            if touches.count == 1 {
                let e = EdgeLock.at(c, in: padSize)
                if e?.isFull == true && s.edge?.isFull != true { out.append(.edgeFull) }
                if e != s.edge { out.append(.edge(e)) }
                s.edge = e
            }
        case .scroll:
            out.append(.scroll(d, s.scrolling ? .changed : .began, at: t, since: since))
            out.append(.press(c))
            s.scrolling = true
        case .core(let scroll):
            if scroll { out.append(.press(c)) }
        case .undecided, .ignored:
            break
        }
        session = s
        return out
    }

    /// A finger lifted. `core`: the core's claim after this event.
    mutating func ended(_ id: Int, at p: CGPoint, time t: TimeInterval, core: CoreClaim = .unclaimed) -> [TrackpadOutput] {
        guard touches.removeValue(forKey: id) != nil, var s = session else { return [] }
        var out = adopt(core, &s)
        if !touches.isEmpty {
            switch s.mode {
            case .pointer where oneFingerScroll && touches.count < 2:
                // The swapped two-finger pointer is over; the rest of this touch does nothing.
                s.mode = .ignored
            default:
                break
            }
            s.anchor = centroid
            s.last = s.anchor
            s.lastAt = t
            if s.edge != nil { s.edge = nil; out.append(.edge(nil)) }
            session = s
            return out
        }
        session = nil
        out.append(.press(nil))
        if s.edge != nil { out.append(.edge(nil)) }
        let duration = t - s.start
        // A second two-finger tap that started within the window of a held-back one: smart zoom,
        // and no right click at all. Any other touch lets the held-back right click go first.
        let twoFingerTap = s.mode == .undecided && s.maxFingers == 2 && duration < Self.twoFingerTapTime
            && (!s.relay || lockedAt == nil)
        let smart = twoFingerTap && rightPending.map { s.start - $0.t <= Self.twoFingerDoubleTapTime } == true
        if !smart { out += fireRight() }
        switch s.mode {
        case .drag:
            if dragLocked || (s.edge?.isFull == true && s.maxFingers == 1) {
                out += lock(at: p)
            } else {
                out.append(.button(.left, down: false))
            }
            return out
        case .scroll:
            out += endMode(s, at: t, cancel: false)
            return out
        case .core:
            // The core ended what it ran; a touch it took is no tap.
            return out
        case .ignored:
            // A locked drag that more fingers joined: the button is still down, it locks again.
            if dragLocked && lockedAt == nil { out += lock(at: p) }
            return out
        case .pointer:
            return out
        case .undecided:
            break
        }
        if s.maxFingers == 1 && duration < Self.tapTime {
            // A tap while a dragging finger is locked is not sent; the lock stays (§4.2).
            guard !s.relay || lockedAt == nil else { return out }
            out.append(.tapFeedback)
            if s.afterTap {
                // Second tap within the window: complete the first click, click again.
                out += finishTap()
                out += [.button(.left, down: true), .button(.left, down: false), .ripple(.double, at: s.startPoint)]
            } else if oneFingerScroll {
                // One finger scrolls: there is no tap-to-drag, so the click is not held back.
                let double = lastClick.map { t - $0 < Self.doubleClickFeedback } ?? false
                lastClick = double ? nil : t
                out += [.button(.left, down: true), .button(.left, down: false), .ripple(double ? .double : .single, at: s.startPoint)]
            } else {
                tapPending = t
                out += [.button(.left, down: true), .ripple(.single, at: s.startPoint), .startTapWindow]
            }
        } else if s.maxFingers == 2 && duration < Self.twoFingerTapTime {
            guard !s.relay || lockedAt == nil else { return out }
            out.append(.tapFeedback)
            if smart {
                rightPending = nil
                out.append(.gesture(.smartMagnify))
            } else {
                // Held back for a possible second two-finger tap (smart zoom).
                rightPending = (t, s.startPoint)
                out.append(.startRightTapWindow)
            }
        } else if s.afterTap {
            // Pressed again after a tap but neither moved nor tapped: the click just completes.
            out += finishTap()
        }
        return out
    }

    /// The drag window after a tap ended: release the button unless a finger is on the pad
    /// deciding between drag and second click (it settles the button itself).
    mutating func tapWindowEnded(at t: TimeInterval) -> [TrackpadOutput] {
        guard let pending = tapPending, t - pending >= Self.dragWindow - 0.001 else { return [] }
        if let s = session, s.afterTap { return [] }
        return finishTap()
    }

    /// The dragging finger lifts with the drag locked: it stays as the locked finger.
    private mutating func lock(at p: CGPoint) -> [TrackpadOutput] {
        dragLocked = true
        lockedAt = p
        return [.locked(p)]
    }

    /// The system took the touches away: nothing stays pressed except a locked drag; a scroll or
    /// gesture is cancelled.
    mutating func cancelled(at t: TimeInterval) -> [TrackpadOutput] {
        guard let s = session else { return [] }
        touches = [:]
        session = nil
        var out: [TrackpadOutput] = [.press(nil)]
        if s.edge != nil { out.append(.edge(nil)) }
        out += endMode(s, at: t, cancel: true)
        if dragLocked && lockedAt == nil {
            out += lock(at: s.last)
        } else if s.mode == .drag {
            out.append(.button(.left, down: false))
        }
        if s.afterTap { out += finishTap() }
        // A held-back right click whose window ran out while this touch was deciding goes now.
        if let pending = rightPending, t - pending.t >= Self.twoFingerDoubleTapTime - 0.001 { out += fireRight() }
        return out
    }

    /// 释放 capsule: the locked finger lifts and the left button goes up; the drag lock ends.
    mutating func releaseLock() -> [TrackpadOutput] {
        guard lockedAt != nil else { return [] }
        lockedAt = nil
        dragLocked = false
        return [.unlocked, .button(.left, down: false)]
    }

    /// Leaving the page: whatever this recognizer holds down goes up, a scroll or gesture is
    /// cancelled.
    mutating func reset(at t: TimeInterval) -> [TrackpadOutput] {
        let down = leftDown
        // A held-back two-finger tap was a finished tap: it still right-clicks.
        let right = fireRight()
        let running = right + (session.map { endMode($0, at: t, cancel: true) } ?? [])
        touches = [:]
        session = nil
        tapPending = nil
        lockedAt = nil
        dragLocked = false
        return running + (down ? [.button(.left, down: false)] : [])
    }

}

/// Selection ticks at a fixed travel distance during a held drag, including drag-lock relays.
/// The caller resets between fingers and feeds only movement classified as dragging.
struct DragTickTravel {
    static let step: CGFloat = 12
    private(set) var remainder: CGFloat = 0
    mutating func reset() { remainder = 0 }
    mutating func move(_ delta: CGVector) -> Int {
        remainder += hypot(delta.dx, delta.dy)
        let ticks = Int(remainder / Self.step)
        remainder -= CGFloat(ticks) * Self.step
        return ticks
    }
}
