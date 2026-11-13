package com.hachiwii.oink

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.assertIsDisplayed
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

/**
 * The control page held sideways = the full screen (docs/control-page.md §1.A, 用户 2026-11-12), light
 * and dark, into build/outputs/roborazzi/{light,dark}/control-land-*.png: the full-screen pad with the
 * floating top bar (visible while the pad is in use or a drag lock holds), the floating keyboard and its
 * gesture states, the expanded keyboard, and 调节 in two columns.
 * The cutout side comes from `flip` (Robolectric has no cutout);
 * the system keyboard itself cannot be shown (no IME), so system-control scenes use synthetic H.
 */
@RunWith(ParameterizedRobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w893dp-h412dp-land-xxhdpi", application = android.app.Application::class)
class ControlLandscapeScreenshotTest(private val dark: Boolean) {
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

    private fun render(control: ControlInit?, state: ControlState) {
        compose.setContent { OinkApp(model, UiInit(tab = Tab.Control, freeze = true, control = control), prefs, {}, AppActions(control = FakeControl(state)), dark) }
        compose.waitForIdle()
    }
    private fun path(name: String) = "build/outputs/roborazzi/${if (dark) "dark" else "light"}/$name.png"
    private fun shot(name: String, control: ControlInit? = null, state: ControlState = state()) {
        render(control, state)
        if (control?.keyboard == null) compose.onNodeWithContentDescription("恢复").assertIsDisplayed()
        compose.onRoot().captureRoboImage(path(name))
    }
    @OptIn(ExperimentalRoborazziApi::class)
    private fun screen(name: String, control: ControlInit, state: ControlState = state()) {
        render(control, state)
        captureScreenRoboImage(path(name))
    }
    private val kb = KeyboardKind.EMULATED

    // Full-screen pad and floating bar (L1 / L2: idle, touching, drag lock).
    @Test fun idle() = shot("control-land")
    @Test fun idleFlip() = shot("control-land-flip", ControlInit(flip = true))
    @Test fun locked() = shot("control-land-locked", state = state(locked = true))
    @Test fun gameOn() = shot("control-land-game-on", state = state(game = ControlGameMode.AUTO, active = true))
    @Test fun touching() = shot("control-land-touching", ControlInit(touch = .45f to .5f))
    @Test fun dragLock() = shot("control-land-drag-lock", ControlInit(edgeDemo = "locked"))
    // Floating keyboard (L1 kb-emulated, F).
    @Test fun keyboard() = shot("control-land-keyboard", ControlInit(keyboard = kb))
    @Test fun keyboardFlip() = shot("control-land-keyboard-flip", ControlInit(keyboard = kb, flip = true))
    @Test fun keyboardMac() = shot("control-land-keyboard-mac", ControlInit(keyboard = kb), state(MAC_MINI))
    @Test fun keyboardSymbols() = shot("control-land-keyboard-symbols", ControlInit(keyboard = kb, symbols = true))
    @Test fun keyboardPreview() = shot("control-land-keyboard-preview", ControlInit(keyboard = kb, preview = "vk-g"))
    @Test fun floatLeft() = shot("control-land-float-left", ControlInit(keyboard = kb, float = "left"))
    @Test fun floatMid() = shot("control-land-float-mid", ControlInit(keyboard = kb, float = "mid"))
    @Test fun floatDismiss() = shot("control-land-float-dismiss", ControlInit(keyboard = kb, float = "dismiss"))
    @Test fun floatPressed() = shot("control-land-float-pressed", ControlInit(keyboard = kb, float = "pressed"))
    // Synthetic IME remaining-height geometry (not a real IME screenshot), including the threshold.
    @Test fun systemTwoRows134() = shot("control-land-system-134", ControlInit(keyboard = KeyboardKind.SYSTEM, systemRemaining = 134f))
    @Test fun systemTwoRows134Fn() = shot("control-land-system-134-fn", ControlInit(keyboard = KeyboardKind.SYSTEM, systemRemaining = 134f, fn = Latch.HELD))
    @Test fun systemThreeRows135() = shot("control-land-system-135", ControlInit(keyboard = KeyboardKind.SYSTEM, systemRemaining = 135f))
    @Test fun systemThreeRows135Fn() = shot("control-land-system-135-fn", ControlInit(keyboard = KeyboardKind.SYSTEM, systemRemaining = 135f, fn = Latch.HELD))
    @Test fun systemTwoRowsMac() = shot("control-land-system-mac-134", ControlInit(keyboard = KeyboardKind.SYSTEM, systemRemaining = 134f), state(MAC_MINI))
    @Test fun systemThreeRowsMac() = shot("control-land-system-mac-135", ControlInit(keyboard = KeyboardKind.SYSTEM, systemRemaining = 135f), state(MAC_MINI))
    // Expanded keyboard (E, E2).
    @Test fun expanded() = shot("control-land-expanded", ControlInit(keyboard = kb, expanded = true))
    @Test fun expandedFlip() = shot("control-land-expanded-flip", ControlInit(keyboard = kb, expanded = true, flip = true))
    @Test fun expandedMac() = shot("control-land-expanded-mac", ControlInit(keyboard = kb, expanded = true), state(MAC_MINI))
    @Test fun expandedShift() = shot("control-land-expanded-shift", ControlInit(keyboard = kb, expanded = true, mods = mapOf(Mod.SHIFT to Latch.HELD)))
    @Test fun expandedFn() = shot("control-land-expanded-fn", ControlInit(keyboard = kb, expanded = true, fn = Latch.HELD))
    @Test fun expandedAudio() { model = model.copy(speakerOn = true, micOn = true); shot("control-land-expanded-audio", ControlInit(keyboard = kb, expanded = true)) }
    // 调节 in two columns (Q) and its second card over the row, two columns for 键盘布局.
    @Test fun adjust() = screen("control-land-adjust", ControlInit(menu = "adjust"))
    @Test fun adjustLayout() = screen("control-land-adjust-layout", ControlInit(menu = "adjust-layout"))
    @Test fun adjustSwipe() = screen("control-land-adjust-swipe", ControlInit(menu = "adjust-swipe"))
    @Test fun gameMenu() = screen("control-land-game-menu", ControlInit(menu = "lock-game"), state(game = ControlGameMode.AUTO, active = true))
}
