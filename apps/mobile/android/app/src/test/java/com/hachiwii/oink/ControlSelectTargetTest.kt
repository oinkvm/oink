package com.hachiwii.oink

import androidx.compose.ui.test.ExperimentalTestApi
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import com.hachiwii.oink.control.*
import com.hachiwii.oink.ui.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.util.Collections

/**
 * Installed 0.4.0-inhouse.4 (2026-11-12): choosing a Windows PC in the control page's device chooser
 * showed 「请先选择要控制的电脑」 and selected nothing (the core had no screen for the PC; fixed in
 * oink-mobile-core, `input_router` tests). Here the page side: the chosen Windows PC goes to the core
 * as the selection, the page follows the core's new focus, and no toast appears.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class ControlSelectTargetTest {
    @get:Rule val compose = createComposeRule()
    private val windowsPc = "a".repeat(64)
    private val macMini = "b".repeat(64)

    /** The core as the page sees it: a selection moves the pointer there and the state names it the focus. */
    private class SelectingControl(private var state: ControlState) : ControlOpener {
        val sent: MutableList<String> = Collections.synchronizedList(ArrayList())
        override fun open(listener: ControlListener): ControlLink {
            listener.onState(state)
            return object : ControlLink {
                override fun setSettings(pointerSpeed: Int, pointsPerMm: Float) {}
                override fun pointerMove(dx: Float, dy: Float, timeUs: Long, sinceUs: Long) {}
                override fun button(button: ControlButton, down: Boolean) {}
                override fun scroll(phase: PadScrollPhase, dx: Float, dy: Float, timeUs: Long, sinceUs: Long) {}
                override fun touches(timeUs: Long, width: Float, height: Float, touches: List<PadContact>, cancelled: Boolean) {}
                override fun gesture(gesture: PadGesture) {}
                override fun key(code: Int, down: Boolean) {}
                override fun inputText(text: String) {}
                override fun movePointerTo(device: String) {
                    sent += "select $device"
                    state = state.copy(focusDevice = device, selectedDevice = device)
                    listener.onState(state)
                }
                override fun setFocusLock(locked: Boolean) {}
                override fun setGameMode(mode: ControlGameMode) {}
                override fun close() {}
                override fun typedChar(layout: String, targetMac: Boolean, ch: Char, mods: Set<Mod>) = UsKeys.typed(layout, targetMac, ch, mods)
            }
        }
    }

    @OptIn(ExperimentalTestApi::class)
    @Test
    fun choosingAWindowsPcSelectsItWithoutTheNoTargetToast() {
        val fake = SelectingControl(ControlState(targets = listOf(
            ControlTarget(windowsPc, "windowsPc", "windows", true), ControlTarget(macMini, "Mac mini", "mac", true))))
        val model = AppModel(selfName = "Phone", peers = listOf(PeerUi(windowsPc, "windowsPc", "windows", latency = 4.0), PeerUi(macMini, "Mac mini", "mac", latency = 6.0)))
        compose.setContent { OinkApp(model, UiInit(tab = Tab.Control, freeze = true), Prefs(scheme = "light"), {}, AppActions(control = fake), false) }
        compose.waitForIdle()
        // Nothing chosen yet: the capsule asks for a device.
        compose.onNodeWithContentDescription("选择设备").performClick()
        compose.waitForIdle()
        compose.onNodeWithText("windowsPc").performClick()
        compose.waitUntil(5_000) { fake.sent.contains("select $windowsPc") }
        // The core's state names it the focus: the capsule shows it; no toast.
        compose.waitUntilAtLeastOneExists(hasContentDescription("焦点设备：windowsPc"), 5_000)
        compose.onAllNodesWithText("请先选择要控制的电脑").assertCountEquals(0)
        compose.onAllNodesWithText("可控制的在线设备").assertCountEquals(0)
    }
}
