package com.hachiwii.oink

import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import com.hachiwii.oink.ui.*
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** Switching tabs: the pages slide together (320 ms) without composing a page in the middle of the slide. */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
// A plain Application: OinkApplication would start the connection service and load the Rust core.
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class TabSwitchTest {
    @get:Rule val compose = createComposeRule()

    private val peers = listOf(PeerUi("windowsPc", "Windows PC", "windows", latency = 4.0), PeerUi("mac", "Mac", "mac", latency = 6.0),
        PeerUi("old", "Old PC", "windows", offline = true))
    private val messages = (1L..60L).map { id ->
        if (id % 3 == 0L) MessageUi(id, false, "windowsPc", "Windows PC", "windows", true, "file", name = "文件$id.pdf", detail = "PDF · 1 MB")
        else MessageUi(id, id % 2 == 0L, if (id % 2 == 0L) "self" else "windowsPc", if (id % 2 == 0L) "Phone" else "Windows PC", if (id % 2 == 0L) "android" else "windows",
            true, "text", text = "第 $id 条消息，" + "内容".repeat((id % 7).toInt() + 1), copyable = true, sent = true)
    }

    private fun app() = compose.setContent {
        OinkApp(AppModel(peers = peers, messages = messages), UiInit(tab = Tab.Devices), Prefs(scheme = "light"), {}, AppActions(), false)
    }

    /**
     * Frame by frame through Devices → Control → Devices (passing the transfer page both ways):
     * prints each frame's composition + layout time (ms) for comparing builds; Robolectric on a PC,
     * so only the relative sizes mean anything.
     */
    @Test fun frameTimesDuringTabSwitch() {
        app()
        // Warm up the JIT and the pages once.
        listOf("控制", "传输", "设备").forEach { compose.onNodeWithContentDescription(it).performClick(); compose.waitForIdle() }
        compose.mainClock.autoAdvance = false
        val runs = ArrayList<List<Double>>()
        repeat(4) { round ->
            compose.onNodeWithContentDescription(if (round % 2 == 0) "控制" else "设备").performClick()
            val frames = ArrayList<Double>()
            repeat(24) {
                val t0 = System.nanoTime()
                compose.mainClock.advanceTimeByFrame()
                compose.waitForIdle()
                frames += (System.nanoTime() - t0) / 1e6
            }
            runs += frames
        }
        runs.forEachIndexed { i, f ->
            println("TABSWITCH run=$i max=%.1f sum=%.1f frames=%s".format(f.max(), f.sum(), f.joinToString(",") { "%.1f".format(it) }))
        }
        val steady = runs.drop(1)
        println("TABSWITCH summary maxOfRuns1to3=%.1f meanSum=%.1f".format(steady.maxOf { it.max() }, steady.map { it.sum() }.average()))
        assertTrue(runs.all { it.isNotEmpty() })
    }

    /** The devices page's bar (gear, speaker, microphone, add) is part of the page: it slides away with it instead of vanishing first. */
    @Test fun theDevicesBarSlidesWithItsPage() {
        app()
        val left = compose.onNodeWithContentDescription("打开设置").getUnclippedBoundsInRoot().left
        val pageWidth = compose.onRoot().getUnclippedBoundsInRoot().right
        compose.mainClock.autoAdvance = false
        compose.onNodeWithContentDescription("传输").performClick()
        compose.mainClock.advanceTimeBy(100)
        compose.waitForIdle()
        // Part way: still there, moved left with the page, its actions with it.
        val moving = compose.onNodeWithContentDescription("打开设置").getUnclippedBoundsInRoot().left
        assertTrue("bar at $moving, was $left", moving < left && moving > left - pageWidth)
        compose.onNodeWithContentDescription("扬声器：关").assertExists()
        compose.onNodeWithContentDescription("添加设备").assertExists()
        // Done: off screen, out of the semantics tree.
        compose.mainClock.advanceTimeBy(400)
        compose.waitForIdle()
        compose.onAllNodesWithContentDescription("打开设置").assertCountEquals(0)
    }

    /** The transfer page stays composed under the other tabs, but does its work only while shown. */
    @Test fun theTransferPageWaitsUntilItIsShown() {
        var left = 0
        var older = 0
        compose.setContent {
            OinkApp(AppModel(peers = peers, messages = messages, hasOlderMessages = true), UiInit(tab = Tab.Devices), Prefs(scheme = "light"), {},
                AppActions(leaveTransfer = { left++ }, loadOlder = { older++ }), false)
        }
        compose.waitForIdle()
        // Under the devices page: no older pages loaded, nothing left.
        assertEquals(0, older)
        assertEquals(0, left)
        compose.onNodeWithContentDescription("传输").performClick()
        compose.waitForIdle()
        assertEquals(0, left)
        compose.onNodeWithContentDescription("设备").performClick()
        compose.waitForIdle()
        assertEquals(1, left)
    }
}
