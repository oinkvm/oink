import Foundation

// Results the focus computer reports back (protocol-v2 §14.6): a pointer move, lock or game-mode
// request returning without an error only means it was sent; the state that comes back decides.
// Inserted text that the focus computer refused comes back with its reason.

enum ControlGame: Equatable { case off, on, auto }

/// What the lock / game button shows (§2.3). Game mode shows only while it is actually on (用户
/// 2026-11-12): 开, or 自动 while it has switched itself on (`gameAuto`, drawn exactly like
/// `gameOn`, no AUTO tag); 自动 while idle shows the pointer lock like 关.
enum LockButtonState: Equatable {
    case unlocked, locked, gameOn, gameAuto

    init(game: ControlGame, gameActive: Bool, locked: Bool) {
        switch game {
        case .on: self = .gameOn
        case .auto where gameActive: self = .gameAuto
        case .auto, .off: self = locked ? .locked : .unlocked
        }
    }

    /// The gamepad is shown (and the button is in its on colours).
    var game: Bool { self == .gameOn || self == .gameAuto }
}

/// What the page asked for, to be seen in `on_state` within `ControlExpectation.window`.
struct ControlExpectation: Equatable {
    static let window: TimeInterval = 1

    var focus: String?
    var locked: Bool?
    var game: ControlGame?

    /// A newer request for the same thing replaces the older one; other parts are kept.
    func merged(with newer: ControlExpectation) -> ControlExpectation {
        ControlExpectation(focus: newer.focus ?? focus, locked: newer.locked ?? locked, game: newer.game ?? game)
    }

    func met(focus current: String?, locked currentLocked: Bool, game currentGame: ControlGame) -> Bool {
        (focus == nil || focus == current) && (locked == nil || locked == currentLocked) && (game == nil || game == currentGame)
    }

    /// The prompt when the state did not follow. `name` gives the device name for an id.
    func failureText(focus current: String?, locked currentLocked: Bool, game currentGame: ControlGame,
                     name: (String) -> String?) -> String? {
        if let focus, focus != current { return "没能把指针移到「\(name(focus) ?? "这台设备")」" }
        if let locked, locked != currentLocked { return locked ? "指针没有锁定" : "指针没有解锁" }
        if let game, game != currentGame { return "游戏模式没有切换" }
        return nil
    }
}

/// Why the focus computer did not insert committed text (`ControlTextRefusal`).
enum TextRefusal: Equatable {
    case paused, notJoined, duplicate, unsupported

    /// The prompt; nil for a duplicate (that text was already inserted, nothing is lost).
    var message: String? {
        switch self {
        case .paused: "目标电脑已暂停共享，文字没有输入"
        case .notJoined: "目标电脑没有加入控制，文字没有输入"
        case .unsupported: "目标设备不支持输入文字"
        case .duplicate: nil
        }
    }
}
