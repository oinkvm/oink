package com.hachiwii.oink

import androidx.activity.BackEventCompat
import androidx.activity.ComponentActivity
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performClick
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class SettingsNavigationTest {
    @get:Rule val compose = createAndroidComposeRule<ComponentActivity>()
    private fun left() = compose.onNodeWithTag("settings-layer").getUnclippedBoundsInRoot().left.value
    private fun open(settings: Boolean = false) {
        compose.setContent { OinkApp(AppModel(), UiInit(settings = settings), Prefs(scheme = "light"), {}, AppActions(), false) }
        compose.waitForIdle()
    }

    @Test fun gearEntersFromLeftAndBackLeavesToLeft() {
        open()
        compose.mainClock.autoAdvance = false
        compose.onNodeWithContentDescription("打开设置").performClick()
        compose.waitForIdle()
        compose.mainClock.advanceTimeBy(64)
        assertTrue("enter x=${left()}", left() < 0f)
        compose.mainClock.advanceTimeBy(400)
        assertEquals(0f, left(), 1f)
        compose.runOnIdle { compose.activity.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
        compose.mainClock.advanceTimeBy(64)
        assertTrue("exit x=${left()}", left() < 0f)
        compose.mainClock.advanceTimeBy(400)
        compose.onAllNodesWithTag("settings-layer").assertCountEquals(0)
    }

    @Test fun predictiveBackMovesLeftAndCancelRestoresThePage() {
        open(settings = true)
        compose.mainClock.autoAdvance = false
        val dispatcher = compose.activity.onBackPressedDispatcher
        compose.runOnIdle { dispatcher.dispatchOnBackStarted(BackEventCompat(0f, 100f, 0f, BackEventCompat.EDGE_LEFT)) }
        compose.waitForIdle()
        compose.runOnIdle { dispatcher.dispatchOnBackProgressed(BackEventCompat(80f, 100f, .4f, BackEventCompat.EDGE_LEFT)) }
        compose.waitForIdle()
        compose.mainClock.advanceTimeByFrame()
        compose.waitForIdle()
        assertTrue("predictive x=${left()}", left() < -100f)
        compose.runOnIdle { dispatcher.dispatchOnBackCancelled() }
        compose.waitForIdle()
        compose.mainClock.advanceTimeBy(400)
        assertEquals(0f, left(), 1f)
        compose.runOnIdle { dispatcher.dispatchOnBackStarted(BackEventCompat(0f, 100f, 0f, BackEventCompat.EDGE_LEFT)) }
        compose.waitForIdle()
        compose.runOnIdle { dispatcher.dispatchOnBackProgressed(BackEventCompat(80f, 100f, .4f, BackEventCompat.EDGE_LEFT)) }
        compose.waitForIdle()
        compose.mainClock.advanceTimeByFrame()
        val beforeCommit = left()
        compose.runOnIdle { dispatcher.onBackPressed() }
        compose.waitForIdle()
        compose.mainClock.advanceTimeBy(64)
        assertTrue("commit keeps moving left", left() < beforeCommit)
        compose.mainClock.advanceTimeBy(400)
        compose.onAllNodesWithTag("settings-layer").assertCountEquals(0)
    }
}
