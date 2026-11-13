package com.hachiwii.oink

import androidx.compose.runtime.*
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createComposeRule
import com.hachiwii.oink.ui.*
import com.hachiwii.oink.control.*
import kotlinx.coroutines.flow.MutableSharedFlow
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class AudioForwardUiTest {
    @get:Rule val compose = createComposeRule()
    @Test fun toolbarHasIndependentTapAndHoldAndPhoneSendSemantics() {
        val calls = mutableListOf<String>()
        compose.setContent { OinkApp(AppModel(forwardOn = true, peers = listOf(PeerUi("phone", "Phone", "android", speaker = "forward"))), UiInit(freeze = true), Prefs(), {},
            AppActions(toggleSpeaker = { calls += "tap" }, longPressSpeaker = { calls += "hold" }), false) }
        compose.onNodeWithContentDescription("扬声器：转发").performClick()
        compose.onNodeWithContentDescription("扬声器：转发").performTouchInput { longClick() }
        assertEquals(listOf("tap", "hold"), calls)
        compose.onNodeWithContentDescription("音频：转发", useUnmergedTree = true).assertExists()
    }
    @Test fun adjustReceiveRowIsOffAndDisabledWhileMicrophoneWorks() {
        val calls = mutableListOf<String>()
        compose.setContent { OinkApp(AppModel(forwardOn = true, speakerOn = true), UiInit(tab = Tab.Control, control = ControlInit(menu = "adjust"), freeze = true), Prefs(), {},
            AppActions(toggleSpeaker = { calls += "speaker" }, toggleMic = { calls += "mic" }), false) }
        compose.onNode(hasText("接收音频")).assertIsNotEnabled().assert(isOff())
        compose.onNode(hasText("接收音频")).performTouchInput { click(); longClick() }
        compose.onNode(hasText("共享麦克风")).performClick()
        assertEquals(listOf("mic"), calls)
    }
    @Config(qualifiers = "w915dp-h412dp-land-xxhdpi")
    @Test fun expandedKeyboardReceiveKeyIsDisabledWhileForwarding() {
        val calls = mutableListOf<String>()
        compose.setContent { OinkApp(AppModel(forwardOn = true), UiInit(tab = Tab.Control, control = ControlInit(keyboard = KeyboardKind.EMULATED, expanded = true), freeze = true), Prefs(), {},
            AppActions(toggleSpeaker = { calls += "tap" }, longPressSpeaker = { calls += "hold" }, toggleMic = { calls += "mic" }), false) }
        compose.onNodeWithContentDescription("接收音频").assertIsNotEnabled().assert(isOff()).performTouchInput { click(); longClick() }
        compose.onNodeWithContentDescription("共享麦克风").performClick()
        assertEquals(listOf("mic"), calls)
    }
    @Config(qualifiers = "w915dp-h412dp-land-xxhdpi")
    @Test fun expandedKeyboardUsesTapAndLongPressWhenNotForwarding() {
        val calls = mutableListOf<String>()
        compose.setContent { OinkApp(AppModel(), UiInit(tab = Tab.Control, control = ControlInit(keyboard = KeyboardKind.EMULATED, expanded = true), freeze = true), Prefs(), {},
            AppActions(toggleSpeaker = { calls += "tap" }, longPressSpeaker = { calls += "hold" }), false) }
        compose.onNodeWithContentDescription("接收音频").performClick().performTouchInput { longClick() }
        assertEquals(listOf("tap", "hold"), calls)
    }
    @Test fun adjustKeepsReceiveActionIndependentOfToolbarTapPreference() {
        val calls = mutableListOf<String>()
        compose.setContent { OinkApp(AppModel(), UiInit(tab = Tab.Control, control = ControlInit(menu = "adjust"), freeze = true), Prefs(speakerTap = SpeakerTap.FORWARD, privileged = "on"), {},
            AppActions(toggleSpeaker = { calls += "toolbar" }, receiveSpeaker = { calls += "receive" }), false) }
        compose.onNode(hasText("接收音频")).performClick()
        assertEquals(listOf("receive"), calls)
    }
    @Test fun settingIsHiddenWithoutPrivilegeEvenWithForwardRemembered() {
        compose.setContent { OinkApp(AppModel(), UiInit(settings = true, settingsScroll = "audio", freeze = true), Prefs(speakerTap = SpeakerTap.FORWARD), {}, AppActions(), false) }
        compose.onNodeWithText("麦克风自动关闭").assertExists()
        compose.onNodeWithText("单击扬声器按钮行为").assertDoesNotExist()
    }
    @Test fun privilegedPickerHasTwoChoicesNoLongPressFootnoteAndPersistsSelection() {
        var prefs by mutableStateOf(Prefs(privileged = "on"))
        compose.setContent { OinkApp(AppModel(), UiInit(settings = true, settingsScroll = "audio", settingsDialog = "picker:speakerTap", freeze = true), prefs, { prefs = it }, AppActions(), false) }
        compose.onAllNodesWithText("设为出口").onLast().assertExists()
        compose.onNodeWithText("转发音频").performClick()
        assertEquals(SpeakerTap.FORWARD, prefs.speakerTap)
        compose.onNodeWithText("长按", substring = true).assertDoesNotExist()
    }
    @Test fun startToastsAreShownInOrderWithoutOverwriting() {
        val messages = MutableSharedFlow<String>(extraBufferCapacity = 8)
        compose.setContent { OinkApp(AppModel(), UiInit(), Prefs(), {}, AppActions(), false, notices = messages) }
        compose.waitForIdle()
        compose.mainClock.autoAdvance = false
        compose.runOnIdle { messages.tryEmit("调节本机音量不影响转发"); messages.tryEmit("转发中本机静音") }
        compose.mainClock.advanceTimeBy(300)
        compose.onNodeWithText("调节本机音量不影响转发").assertExists()
        compose.onNodeWithText("转发中本机静音").assertDoesNotExist()
        compose.mainClock.advanceTimeBy(2100)
        compose.onNodeWithText("转发中本机静音").assertExists()
        compose.onNodeWithText("调节本机音量不影响转发").assertDoesNotExist()
    }
}
