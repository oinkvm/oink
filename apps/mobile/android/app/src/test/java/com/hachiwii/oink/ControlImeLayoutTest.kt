package com.hachiwii.oink

import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.unit.DpSize
import androidx.compose.ui.unit.dp
import com.hachiwii.oink.control.*
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * A tall system keyboard never turns portrait into landscape (user 2026-11-12: Sogou's 超级联想 panel
 * made the IME so tall that the space left above it was wider than high, and the control page switched
 * to the landscape split). The layouts come from the app area without the keyboard.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class ControlImeLayoutTest {
    @get:Rule val compose = createComposeRule()
    private val pc = "a".repeat(64)

    @Test fun layoutsDependOnTheAreaWithoutTheKeyboard() {
        // Portrait phone: a 412 × 395 space above a 520 high IME would have read as landscape; the frame is 412 × 915.
        val portrait = AppLayout(DpSize(412.dp, 915.dp))
        assertFalse(portrait.phoneLandscape); assertFalse(portrait.side); assertTrue(portrait.canFullscreen)
        val sideways = AppLayout(DpSize(893.dp, 412.dp))
        assertTrue(sideways.phoneLandscape); assertTrue(sideways.side)
        val tablet = AppLayout(DpSize(1280.dp, 800.dp))
        assertFalse(tablet.phoneLandscape); assertTrue(tablet.side); assertFalse(tablet.canFullscreen)
    }

    @Test fun portraitStaysPortraitWhileTheImeGrowsTo520() {
        val ime = mutableIntStateOf(0)
        val fake = FakeControl(ControlState(focusDevice = pc, selectedDevice = pc, targets = listOf(ControlTarget(pc, "Windows PC", "windows", true))))
        val model = AppModel(selfName = "Phone", peers = listOf(PeerUi(pc, "Windows PC", "windows", latency = 4.0)))
        compose.setContent {
            CompositionLocalProvider(LocalImeLift provides ImeLift { ime.intValue }) {
                OinkApp(model, UiInit(tab = Tab.Control, freeze = true), Prefs(scheme = "light"), {}, AppActions(control = fake), false)
            }
        }
        compose.waitForIdle()
        val padBefore = compose.onNodeWithContentDescription("触摸板").getUnclippedBoundsInRoot()
        ime.intValue = with(compose.density) { 520.dp.roundToPx() }
        compose.waitForIdle()
        // Still the portrait page: 全屏 top right, no landscape bar (恢复), the tabs not moved into the page.
        compose.onNodeWithContentDescription("全屏").assertExists()
        compose.onAllNodesWithContentDescription("恢复").assertCountEquals(0)
        // The pad keeps the full width (no landscape split); only its height gives way.
        val pad = compose.onNodeWithContentDescription("触摸板").getUnclippedBoundsInRoot()
        assertTrue("pad $pad, was $padBefore", kotlin.math.abs((pad.left - padBefore.left).value) < 1f && kotlin.math.abs((pad.right - padBefore.right).value) < 1f)
        assertTrue("pad $pad, was $padBefore", pad.bottom < padBefore.bottom)
    }
}
