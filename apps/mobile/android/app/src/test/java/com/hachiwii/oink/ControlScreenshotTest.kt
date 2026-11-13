package com.hachiwii.oink

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import com.github.takahirom.roborazzi.ExperimentalRoborazziApi
import com.github.takahirom.roborazzi.captureRoboImage
import com.github.takahirom.roborazzi.captureScreenRoboImage
import com.hachiwii.oink.control.*
import com.hachiwii.oink.ui.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.ParameterizedRobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** Test double of the core's control session: reports a fixed state and records what the page sends. */
class FakeControl(private val state: ControlState) : ControlOpener {
    val sent = ArrayList<String>()
    val contacts = ArrayList<List<PadContact>>()
    override fun open(listener: ControlListener): ControlLink {
        listener.onState(state)
        return object : ControlLink {
            override fun setSettings(pointerSpeed: Int, pointsPerMm: Float) { sent += "settings $pointerSpeed" }
            override fun pointerMove(dx: Float, dy: Float, timeUs: Long, sinceUs: Long) { sent += "move" }
            override fun button(button: ControlButton, down: Boolean) { sent += "button $button $down" }
            override fun scroll(phase: PadScrollPhase, dx: Float, dy: Float, timeUs: Long, sinceUs: Long) { sent += "scroll" }
            override fun touches(timeUs: Long, width: Float, height: Float, touches: List<PadContact>, cancelled: Boolean) { contacts += touches.toList() }
            override fun gesture(gesture: PadGesture) { sent += "gesture" }
            override fun key(code: Int, down: Boolean) { sent += "key ${Integer.toHexString(code)} $down" }
            override fun inputText(text: String) { sent += "text $text" }
            override fun movePointerTo(device: String) { sent += "select $device" }
            override fun setFocusLock(locked: Boolean) { sent += "lock $locked" }
            override fun setGameMode(mode: ControlGameMode) { sent += "game $mode" }
            override fun close() { sent += "close" }
            override fun typedChar(layout: String, targetMac: Boolean, ch: Char, mods: Set<Mod>) = UsKeys.typed(layout, targetMac, ch, mods)
        }
    }
}

/**
 * Control page states (docs/control-page.md §8) in light and dark, rendered under Robolectric into
 * build/outputs/roborazzi/{light,dark}/control-*.png. The system keyboard itself cannot be shown
 * here (no IME under Robolectric), so its accessory-bar state is not among them. Portrait; the
 * landscape states are in [ControlLandscapeScreenshotTest].
 */
@RunWith(ParameterizedRobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class ControlScreenshotTest(private val dark: Boolean) {
    companion object {
        @JvmStatic @ParameterizedRobolectricTestRunner.Parameters(name = "dark={0}")
        fun themes() = listOf(arrayOf<Any>(false), arrayOf<Any>(true))
        private val WINDOWS_PC = "a".repeat(64)
        private val MAC_MINI = "b".repeat(64)
    }

    @get:Rule val compose = createComposeRule()

    private val targets = listOf(ControlTarget(WINDOWS_PC, "Windows PC", "windows", true), ControlTarget(MAC_MINI, "Mac mini", "mac", true))
    private var model = AppModel(selfName = "Phone", peers = listOf(
        PeerUi(WINDOWS_PC, "Windows PC", "windows", latency = 4.0), PeerUi(MAC_MINI, "Mac mini", "mac", latency = 6.0)))
    private val prefs = Prefs(scheme = if (dark) "dark" else "light")
    private fun state(focus: String = WINDOWS_PC, locked: Boolean = false, game: ControlGameMode = ControlGameMode.OFF, active: Boolean = false) =
        ControlState(focusDevice = focus, selectedDevice = focus, locked = locked, gameMode = game, gameActive = active, targets = targets)

    private fun render(control: ControlInit?, state: ControlState): FakeControl {
        val fake = FakeControl(state)
        compose.setContent { OinkApp(model, UiInit(tab = Tab.Control, freeze = true, control = control), prefs, {}, AppActions(control = fake), dark) }
        compose.waitForIdle()
        return fake
    }
    private fun path(name: String) = "build/outputs/roborazzi/${if (dark) "dark" else "light"}/$name.png"
    private fun shot(name: String, control: ControlInit? = null, state: ControlState = state()) {
        render(control, state)
        compose.onRoot().captureRoboImage(path(name))
    }
    /** Menus are popup windows: the whole screen is captured. */
    @OptIn(ExperimentalRoborazziApi::class)
    private fun screen(name: String, control: ControlInit, state: ControlState = state()) {
        render(control, state)
        captureScreenRoboImage(path(name))
    }

    @Test fun control() { shot("control") }
    @Test fun noTarget() = shot("control-no-target", state = ControlState(noTarget = true))
    @Test fun adjust() = screen("control-adjust", ControlInit(menu = "adjust"))
    @Test fun swipe() = screen("control-swipe", ControlInit(menu = "adjust-swipe"))
    @Test fun layout() = screen("control-layout", ControlInit(menu = "adjust-layout"))
    @Test fun focus() = screen("control-focus", ControlInit(menu = "focus"))
    /** 调节 with its switches on (用户 2026-11-12): 接收音频 / 共享麦克风 come from the device settings. */
    @Test fun adjustOn() { model = model.copy(speakerOn = true, micOn = true); screen("control-adjust-on", ControlInit(menu = "adjust")) }
    @Test fun locked() = shot("control-locked", state = state(locked = true))
    @Test fun gameOn() = shot("control-game-on", state = state(game = ControlGameMode.ON, active = true))
    /** 自动 and on now looks exactly like 开 (no AUTO tag, 用户 2026-11-12). */
    @Test fun gameAuto() = shot("control-game-auto", state = state(game = ControlGameMode.AUTO, active = true))
    /** 自动 while off: the lock state shows as usual. */
    @Test fun gameAutoIdle() = shot("control-game-auto-idle", state = state(game = ControlGameMode.AUTO, active = false))
    @Test fun lockMenu() = screen("control-lock-menu", ControlInit(menu = "lock"))
    @Test fun lockMenuLocked() = screen("control-lock-menu-locked", ControlInit(menu = "lock"), state(locked = true))
    @Test fun gameMenu() = screen("control-game-menu", ControlInit(menu = "lock-game"), state(game = ControlGameMode.AUTO, active = true))
    @Test fun keyboard() = shot("control-keyboard", ControlInit(keyboard = KeyboardKind.EMULATED))
    /** Keyboard up: 切换键盘 in the header's top-right slot, 收起键盘 right of ↑, number row on both layers (用户 2026-11-12). */
    @Test fun keyboardSymbols() = shot("control-keyboard-symbols", ControlInit(keyboard = KeyboardKind.EMULATED, symbols = true))
    @Test fun keyboardSymbolsShift() = shot("control-keyboard-symbols-shift", ControlInit(keyboard = KeyboardKind.EMULATED, symbols = true, mods = mapOf(Mod.SHIFT to Latch.HELD)))
    @Test fun keyboardMac() = shot("control-keyboard-mac", ControlInit(keyboard = KeyboardKind.EMULATED), state(MAC_MINI))
    @Test fun keyboardFn() = shot("control-keyboard-fn", ControlInit(keyboard = KeyboardKind.EMULATED, fn = Latch.HELD))
    @Test fun keyboardFnLocked() = shot("control-keyboard-fn-locked", ControlInit(keyboard = KeyboardKind.EMULATED, fn = Latch.LOCKED))
    @Test fun keyboardMacFn() = shot("control-keyboard-mac-fn", ControlInit(keyboard = KeyboardKind.EMULATED, fn = Latch.HELD), state(MAC_MINI))
    /** System keyboard + Ctrl tapped / long-pressed: the emulated keyboard stands in. */
    @Test fun keyboardAutoHeld() = shot("control-keyboard-auto-held", ControlInit(keyboard = KeyboardKind.SYSTEM, mods = mapOf(Mod.CTRL to Latch.HELD)))
    @Test fun keyboardAutoLocked() = shot("control-keyboard-auto-locked", ControlInit(keyboard = KeyboardKind.SYSTEM, mods = mapOf(Mod.CTRL to Latch.LOCKED)))
    @Test fun keyboardMods() = shot("control-keyboard-mods", ControlInit(keyboard = KeyboardKind.EMULATED, mods = mapOf(Mod.CTRL to Latch.HELD, Mod.SHIFT to Latch.LOCKED)))
    @Test fun keyboardShiftHeld() = shot("control-keyboard-shift-held", ControlInit(keyboard = KeyboardKind.EMULATED, mods = mapOf(Mod.SHIFT to Latch.HELD)))
    @Test fun keyboardShiftLocked() = shot("control-keyboard-shift-locked", ControlInit(keyboard = KeyboardKind.EMULATED, mods = mapOf(Mod.SHIFT to Latch.LOCKED)))
    // Key preview bubbles (§3.1.4). Robolectric has no IME, so the control-strip states use the emulated keyboard's strip.
    @Test fun keyboardPreview() = shot("control-keyboard-preview", ControlInit(keyboard = KeyboardKind.EMULATED, preview = "vk-g"))
    @Test fun keyboardPreviewEdge() = shot("control-keyboard-preview-edge", ControlInit(keyboard = KeyboardKind.EMULATED, preview = "vk-q"))
    @Test fun keyboardPreviewSpace() = shot("control-keyboard-preview-space", ControlInit(keyboard = KeyboardKind.EMULATED, preview = "space"))
    @Test fun keyboardPreviewMulti() = shot("control-keyboard-preview-multi", ControlInit(keyboard = KeyboardKind.EMULATED, preview = "vk-d", held = listOf("vk-a")))
    @Test fun stripPreview() = shot("control-strip-preview", ControlInit(keyboard = KeyboardKind.EMULATED, preview = "copy"))
    @Test fun stripPreviewEdge() = shot("control-strip-preview-edge", ControlInit(keyboard = KeyboardKind.EMULATED, preview = "esc"))
    @Test fun stripPreviewWide() = shot("control-strip-preview-wide", ControlInit(keyboard = KeyboardKind.EMULATED, preview = "pgdn"))
    @Test fun stripPreviewArrow() = shot("control-strip-preview-arrow", ControlInit(keyboard = KeyboardKind.EMULATED, preview = "right"))
    @Test fun stripPreviewFKey() = shot("control-strip-preview-fkey", ControlInit(keyboard = KeyboardKind.EMULATED, fn = Latch.LOCKED, preview = "f10"))
    @Test fun touching() = shot("control-touching", ControlInit(touch = .45f to .5f))
    @Test fun edgeGlow() = shot("control-edge-glow", ControlInit(edgeDemo = "half"))
    @Test fun edgeFull() = shot("control-edge-full", ControlInit(edgeDemo = "full"))
    @Test fun edgeCorner() = shot("control-edge-corner", ControlInit(edgeDemo = "corner"))
    @Test fun edgeLocked() = shot("control-edge-locked", ControlInit(edgeDemo = "locked"))
    @Test fun edgeRelay() = shot("control-edge-relay", ControlInit(edgeDemo = "relay"))
}
