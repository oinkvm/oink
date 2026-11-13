package com.hachiwii.oink

import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** Settings › 传输 › 清理传输记录 (settings-and-pairing.md §3.3, user 2026-11-11). */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
// A plain Application: OinkApplication would start the connection service and load the Rust core.
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class ClearTransferHistoryTest {
    @get:Rule val compose = createComposeRule()

    private val message = MessageUi(1, false, "windowsPc", "Windows PC", "windows", true, "text", text = "你好", copyable = true)
    private var calls = 0

    private fun settings(messages: List<MessageUi>, result: String? = null) = compose.setContent {
        OinkApp(AppModel(messages = messages), UiInit(settings = true), Prefs(scheme = "light", cacheBytes = 4096), {},
            AppActions(clearHistory = { calls++; result }), false)
    }

    private fun row() = compose.onNodeWithContentDescription("清理传输记录").performScrollTo()

    @Test fun rowIsGreyWhenThereAreNoRecords() {
        settings(emptyList())
        row().assertIsNotEnabled()
        // The cache row is still its own action below it.
        compose.onNodeWithContentDescription("清理缓存，4 KB").assertIsEnabled()
        compose.onNodeWithText("清理缓存会删除已下载的图片和文件，消息记录会保留。").assertExists()
        compose.onAllNodesWithText("图片先显示预览", substring = true).assertCountEquals(0)
    }

    @Test fun confirmingClearsAndShowsTheToast() {
        settings(listOf(message))
        row().assertIsEnabled().performClick()
        compose.onNodeWithText("清理传输记录？").assertExists()
        compose.onNodeWithText("将删除本机的所有传输记录，以及已下载的图片和文件。其它设备上的记录不受影响。").assertExists()
        // 取消 closes the dialog without clearing.
        compose.onNodeWithText("取消").performClick()
        compose.onAllNodesWithText("清理传输记录？").assertCountEquals(0)
        assertEquals(0, calls)

        row().performClick()
        compose.onNodeWithText("清理传输记录？").assertExists()
        // Held clock from here: the toast stays up for its 2 s.
        compose.mainClock.autoAdvance = false
        compose.onNodeWithText("清理").performClick()
        compose.mainClock.advanceTimeBy(200)
        assertEquals(1, calls)
        compose.onAllNodesWithText("清理传输记录？").assertCountEquals(0)
        compose.onNodeWithText("已清理传输记录").assertExists()
    }

    @Test fun aFailedClearShowsItsReason() {
        settings(listOf(message), result = "无法清理传输记录")
        row().performClick()
        compose.onNodeWithText("清理传输记录？").assertExists()
        // Held clock from here: the toast stays up for its 2 s.
        compose.mainClock.autoAdvance = false
        compose.onNodeWithText("清理").performClick()
        compose.mainClock.advanceTimeBy(200)
        assertEquals(1, calls)
        compose.onNodeWithText("无法清理传输记录").assertExists()
        compose.onAllNodesWithText("已清理传输记录").assertCountEquals(0)
    }
}
