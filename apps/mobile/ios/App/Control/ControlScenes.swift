#if DEBUG
import SwiftUI

/// Control page design states for the screenshots (`-oink-ui-scene control-*`, DEBUG only;
/// names as in docs control-page.md §8). They set what the views read; no control handle is
/// opened and nothing is sent.
@MainActor
enum ControlScenes {
    private static var scene: String { DesignScene.current ?? "" }
    private static var isControl: Bool { scene.hasPrefix("control") }

    /// The 调节 menu. Its second levels (control-swipe / -orient / -layout) are system menus,
    /// which a design state cannot open: ControlPageUITests opens them from control-adjust.
    static var adjustOpen: Bool { scene == "control-adjust" }
    /// Synthetic threshold scenes; the OS keyboard stays real and no settings change.
    static var systemHeightOverride: CGFloat? {
        if scene.contains("-h134") { return 134 }
        if scene.contains("-h135") { return 135 }
        return nil
    }
    static var freezeRipples: Bool { scene.hasPrefix("control-ripple") }

    static func install(_ model: ControlModel) {
        guard isControl else { return }
        let mac = scene.contains("-mac")
        var snapshot = ControlSnapshot(
            focusDevice: mac ? "fixture-mac" : "fixture-pc", selectedDevice: nil, noTarget: false,
            targets: [
                ControlTargetInfo(device: "fixture-pc", name: "Office-PC", platform: "windows", online: true),
                ControlTargetInfo(device: "fixture-mac", name: "MacBook-Pro", platform: "macos", online: true),
            ])
        switch scene {
        case "control-locked": snapshot.locked = true
        case "control-game-on": snapshot.gameMode = .on; snapshot.gameActive = true
        case "control-game-auto": snapshot.gameMode = .auto; snapshot.gameActive = true
        // 自动 while game mode has not switched itself on: the button shows the lock (用户 2026-11-12).
        case "control-game-auto-idle": snapshot.gameMode = .auto; snapshot.gameActive = false
        case "control-empty": snapshot = ControlSnapshot()
        default: break
        }
        guard scene.hasPrefix("control-keyboard") || scene.hasPrefix("control-strip") else {
            model.installScene(snapshot: snapshot, keyboard: nil)
            if adjustOpen { model.adjustOpen = true }
            return
        }
        let system = scene.hasPrefix("control-keyboard-system") || scene.hasPrefix("control-strip")
        var mods: [Modifier: ModifierState] = [:]
        var fn: ModifierState?
        switch scene {
        case "control-keyboard-auto-held": mods = [.ctrl: .held]
        case "control-keyboard-auto-locked": mods = [.ctrl: .locked]
        case "control-keyboard-mods": mods = [.ctrl: .held, .shift: .locked]
        case "control-keyboard-shift-held", "control-keyboard-symbols-shift", "control-keyboard-expanded-shift-held": mods = [.shift: .held]
        case "control-keyboard-shift-locked": mods = [.shift: .locked]
        case "control-keyboard-fn", "control-keyboard-mac-fn": fn = .held
        case "control-keyboard-fn-locked", "control-keyboard-mac-fn-locked", "control-strip-preview-fkey": fn = .locked
        default: break
        }
        let preferred: KeyboardKind = system || scene.hasPrefix("control-keyboard-auto") ? .system : .emulated
        model.installScene(snapshot: snapshot, keyboard: system ? .system : .emulated, preferred: preferred,
                           modifiers: mods, fn: fn, symbols: scene.contains("symbols"),
                           // control-keyboard-expanded: the landscape Oink keyboard over the whole screen.
                           expanded: scene.contains("expanded"))
        installPreview(model)
    }

    /// Key preview states (§3.1.4, §8): the key a finger rests on (with its bubble) and keys
    /// other fingers hold. `control-strip-*` are the system keyboard's accessory bar.
    private static var preview: (key: String, held: [String])? {
        switch scene {
        case "control-keyboard-preview": ("vk-g", [])
        case "control-keyboard-preview-edge": ("vk-q", [])
        case "control-keyboard-preview-space": ("space", [])
        case "control-keyboard-preview-multi": ("vk-d", ["vk-a"])
        case "control-strip-preview": ("copy", [])
        case "control-strip-preview-edge": ("esc", [])
        case "control-strip-preview-wide": ("pgdn", [])
        case "control-strip-preview-arrow": ("right", [])
        case "control-strip-preview-fkey": ("f10", [])
        default: nil
        }
    }

    /// Puts the fingers down once the keyboard has risen and its keys are laid out.
    private static var previewDone = false
    private static func installPreview(_ model: ControlModel) {
        guard let preview, !previewDone else { return }
        previewDone = true
        // Wait until the key is laid out and has stopped moving (the keyboard rises first; a
        // fixed 1.5 s was too early for one of the integration screenshots), at most 10 s.
        var last: CGRect?
        var still = 0
        func check(_ tries: Int) {
            let frame = model.keySpots.spot(preview.key)?.frame
            still = frame != nil && frame == last ? still + 1 : 0
            last = frame
            guard still >= 3 || tries >= 100 else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { check(tries + 1) }
                return
            }
            for (i, id) in preview.held.enumerated() {
                if let key = model.keySpots.spot(id)?.slide { model.slide.press(-100 - i, key) }
            }
            if let key = model.keySpots.spot(preview.key)?.slide { model.slide.press(-1, key) }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { check(0) }
    }

    /// Edge-lock and touch states, placed once the pad has its size.
    static func installPad(_ driver: TrackpadDriver, size: CGSize) {
        guard isControl, size.width > 0 else { return }
        let w = size.width, h = size.height
        let finger: CGPoint? = switch scene {
        case "control-edge-glow": CGPoint(x: w - 40, y: h * 0.42)
        case "control-edge-full": CGPoint(x: w - 12, y: h * 0.42)
        case "control-edge-corner": CGPoint(x: w - 34, y: h - 30)
        case "control-edge-relay": CGPoint(x: w * 0.35, y: h * 0.55)
        case "control-touching": CGPoint(x: w * 0.5, y: h * 0.45)
        default: nil
        }
        let edge = finger.flatMap { scene == "control-edge-relay" ? nil : EdgeLock.at($0, in: size) }
        let locked = scene == "control-edge-locked" || scene == "control-edge-relay"
        let ripple: Ripple? = switch scene {
        case "control-ripple": Ripple(kind: .single, at: CGPoint(x: w * 0.5, y: h * 0.4))
        case "control-ripple-double": Ripple(kind: .double, at: CGPoint(x: w * 0.5, y: h * 0.4))
        case "control-ripple-right": Ripple(kind: .right, at: CGPoint(x: w * 0.5, y: h * 0.4))
        default: nil
        }
        driver.installScene(press: finger, edge: edge, locked: locked, ripple: ripple)
    }
}
#endif
