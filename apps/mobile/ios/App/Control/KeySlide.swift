import Combine
import CoreGraphics
import Foundation

// Sliding between keys and several fingers on the control keyboards (docs control-page.md
// §3.1.4, user 2026-11-11). Pure state, no UI: the keyboard's touch view feeds it fingers, the
// keys' own actions go out through the model.

/// Keys sent down and not yet up (按下集合), each once, with what letting it go needs.
struct PressedKeys: Equatable {
    struct Entry: Equatable {
        /// The key and the Shift / AltGr (Option) that went down around it.
        var key: TypedKey
        /// False for media keys: they neither take nor release the modifiers.
        var combines: Bool
        var code: UInt32 { key.code }
    }
    private(set) var entries: [String: Entry] = [:]

    /// No key that combines with modifiers is down: held modifiers may let go.
    var idle: Bool { !entries.values.contains { $0.combines } }

    /// A held key (arrows, rewind / fast-forward, volume, space, backspace, return) goes down.
    mutating func holdDown(_ id: String, code: UInt32, combines: Bool) -> [KeyEvent] {
        guard entries[id] == nil else { return [] }
        entries[id] = Entry(key: TypedKey(code: code), combines: combines)
        return [KeyEvent(code: code, down: true)]
    }

    /// A character key goes down, wrapped in the Shift / AltGr (Option) it needs and the user
    /// does not hold (§3.1.2; decided by `KeyLayouts.typed`). Not for dead keys: they are tapped.
    mutating func typedDown(_ id: String, _ key: TypedKey) -> [KeyEvent] {
        guard entries[id] == nil else { return [] }
        entries[id] = Entry(key: key, combines: true)
        return key.downEvents
    }

    /// The key goes up. Held modifiers let go once no combining key is down any more (several
    /// fingers, user 2026-11-11): after the up that empties the set.
    mutating func up(_ id: String, board: inout ModifierBoard) -> [KeyEvent] {
        guard let e = entries.removeValue(forKey: id) else { return [] }
        var out = e.key.upEvents
        if e.combines && idle { out += board.releaseHeld() }
        return out
    }

    /// After a combining key's down + up at once (control-area click keys, copy / paste): held
    /// modifiers let go unless another finger still holds a combining key.
    func afterClick(board: inout ModifierBoard) -> [KeyEvent] { idle ? board.releaseHeld() : [] }

    /// The keyboard closes: every key goes up.
    mutating func releaseAll() -> [KeyEvent] {
        var out: [KeyEvent] = []
        for e in entries.values { out += e.key.upEvents }
        entries = [:]
        return out
    }
}

/// Auto-repeat of a held key (§3.1 按住重复, user 2026-11-12; the same rules as Android's
/// KeyboardModel). Computers never repeat a key on their own: a desktop source's OS repeats a held
/// key and Oink forwards each repeat (oink-routing `forward_key_repeat`, `KeyRepeat`). The phone
/// does the same: each tick sends the held key's down again, which the phone's router turns into
/// `KeyRepeat` for the computer that got the press. As on a hardware keyboard only the newest
/// held key repeats, and an older one does not start again when it stops. iOS has no key-repeat
/// setting to read: first repeat after 500 ms (Windows' default delay), then every 50 ms.
final class KeyRepeater {
    static let delay: TimeInterval = 0.5
    static let interval: TimeInterval = 0.05

    /// Starts calling `tick` after `delay`, then every `interval`; returns the call that stops it.
    typealias Schedule = (_ delay: TimeInterval, _ interval: TimeInterval, _ tick: @escaping () -> Void) -> () -> Void

    private let schedule: Schedule
    private var cancel: (() -> Void)?
    /// The key repeating now.
    private(set) var key: String?

    init(schedule: @escaping Schedule = KeyRepeater.mainRunLoop) { self.schedule = schedule }

    /// `id` just went down: it repeats from now on, and whatever repeated before stops.
    func start(_ id: String, tick: @escaping (String) -> Void) {
        stopAll()
        key = id
        cancel = schedule(Self.delay, Self.interval) { tick(id) }
    }

    /// `id` went up: its repeat stops (another key's repeat goes on).
    func stop(_ id: String) { if key == id { stopAll() } }

    func stopAll() {
        cancel?()
        cancel = nil
        key = nil
    }

    /// A timer on the main run loop in the common modes, so it keeps firing while UIKit tracks
    /// touches. Missed ticks are skipped, not fired in a burst.
    static func mainRunLoop(_ delay: TimeInterval, _ interval: TimeInterval, _ tick: @escaping () -> Void) -> () -> Void {
        let timer = Timer(fire: Date(timeIntervalSinceNow: delay), interval: interval, repeats: true) { _ in tick() }
        RunLoop.main.add(timer, forMode: .common)
        return { timer.invalidate() }
    }
}

/// A key with a preview bubble, which fingers can slide onto. `hold` keys go down when the
/// first finger enters and up when the last one leaves; the others do nothing on entering or
/// leaving and `fire` once (down + up) when the last finger lifts on them.
struct SlideKey {
    let id: String
    let hold: Bool
    var down: () -> Void = {}
    var up: () -> Void = {}
    var fire: () -> Void = {}
}

/// What is under a finger: a key with a bubble, nothing (a modifier, Fn, a gap far from
/// keys…), or outside the keyboard.
enum SlideHit {
    case on(SlideKey)
    case none
    case out
}

/// The bubble: the key it is on, and whether it glides there from the key it was on.
struct SlideBubble {
    let key: SlideKey
    let glide: Bool
}

/// Fingers on the keyboard. Each finger has its own key under it; sliding to another key =
/// leaving the old one, then entering the new one. A key is down while at least one finger is
/// on it. Leaving the keyboard ends the finger for keys: its key is left (a hold key goes up, a
/// tap key is cancelled) and it presses nothing more until it lifts. The bubble belongs to the
/// finger that came down last; it does not jump back to an older finger, which gets it again
/// when it next moves to another key. `feel` is the haptic for a key going down (false: the
/// first touch, true: a slide).
final class KeySlide: ObservableObject {
    private final class Finger {
        var key: SlideKey?
        var dead = false
    }
    private var fingers: [Int: Finger] = [:]
    private var order: [Int] = []
    private var count: [String: Int] = [:]
    private var newest: Int?
    var feel: (Bool) -> Void = { _ in }

    /// Keys with a finger on them (drawn pressed).
    @Published private(set) var down: Set<String> = []
    @Published private(set) var bubble: SlideBubble?

    func tracks(_ finger: Int) -> Bool { fingers[finger] != nil }
    func keyOf(_ finger: Int) -> String? { fingers[finger]?.key?.id }

    /// A finger lands on `key`.
    func press(_ finger: Int, _ key: SlideKey) {
        let f = Finger()
        fingers[finger] = f
        order.append(finger)
        newest = finger
        enter(f, key, slide: false)
        bubble = SlideBubble(key: key, glide: false)
    }

    func move(_ finger: Int, _ hit: SlideHit) {
        guard let f = fingers[finger], !f.dead else { return }
        switch hit {
        case .out:
            leave(f, lifted: false)
            f.dead = true
            if newest == finger { bubble = nil }
        case .none:
            guard f.key != nil else { return }
            leave(f, lifted: false)
            if newest == finger { bubble = nil }
        case .on(let key):
            guard key.id != f.key?.id else { return }
            let had = f.key != nil
            leave(f, lifted: false)
            enter(f, key, slide: true)
            if newest == finger { bubble = SlideBubble(key: key, glide: had && bubble != nil) }
        }
    }

    /// The finger lifts: a tap key under it fires if it was the last finger on it.
    func lift(_ finger: Int) { end(finger, lifted: true) }

    /// The system took the touch away: like leaving, nothing fires.
    func cancel(_ finger: Int) { end(finger, lifted: false) }

    private func end(_ finger: Int, lifted: Bool) {
        guard let f = fingers.removeValue(forKey: finger) else { return }
        order.removeAll { $0 == finger }
        leave(f, lifted: lifted && !f.dead)
        if newest == finger {
            newest = order.last
            bubble = nil
        }
    }

    /// The keys under the fingers changed (Fn, 123 / ABC): each finger's key is left; its next
    /// move finds the new one.
    func relayout() {
        for (id, f) in fingers where f.key != nil {
            leave(f, lifted: false)
            if newest == id { bubble = nil }
        }
    }

    /// The keyboard closed or switched: every finger is cancelled.
    func reset() { for id in order { cancel(id) } }

    private func enter(_ f: Finger, _ key: SlideKey, slide: Bool) {
        f.key = key
        let n = (count[key.id] ?? 0) + 1
        count[key.id] = n
        guard n == 1 else { return } // already down under another finger: nothing new goes out
        down.insert(key.id)
        feel(slide)
        if key.hold { key.down() }
    }

    private func leave(_ f: Finger, lifted: Bool) {
        guard let key = f.key else { return }
        f.key = nil
        let n = (count[key.id] ?? 1) - 1
        if n > 0 { count[key.id] = n; return } // another finger still holds it
        count[key.id] = nil
        down.remove(key.id)
        if key.hold { key.up() } else if lifted { key.fire() }
    }

    /// The key under a point: outside every area = `.out`; `current` is kept while the point is
    /// within `stick` of it; otherwise the nearest key within `near` (gaps belong to the keys on
    /// both sides), `.none` when that key has no bubble (nil) or nothing is that near.
    static func locate(_ p: CGPoint, current: String?, keys: [(CGRect, SlideKey?)], areas: [CGRect],
                       near: CGFloat, stick: CGFloat) -> SlideHit {
        guard areas.contains(where: { $0.insetBy(dx: -0.5, dy: -0.5).contains(p) }) else { return .out }
        func d(_ r: CGRect) -> CGFloat {
            hypot(max(r.minX - p.x, 0, p.x - r.maxX), max(r.minY - p.y, 0, p.y - r.maxY))
        }
        if let current, let match = keys.first(where: { $0.1?.id == current }), let k = match.1, d(match.0) <= stick { return .on(k) }
        guard let best = keys.min(by: { d($0.0) < d($1.0) }), d(best.0) < near else { return .none }
        return best.1.map { .on($0) } ?? .none
    }
}
