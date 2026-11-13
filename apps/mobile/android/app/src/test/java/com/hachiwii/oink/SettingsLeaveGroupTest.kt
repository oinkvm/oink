package com.hachiwii.oink

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import com.hachiwii.oink.ui.Accent
import com.hachiwii.oink.ui.AppActions
import com.hachiwii.oink.ui.AppModel
import com.hachiwii.oink.ui.OinkApp
import com.hachiwii.oink.ui.OinkColors
import com.hachiwii.oink.ui.OinkTheme
import com.hachiwii.oink.ui.Prefs
import com.hachiwii.oink.ui.SettingsPage
import com.hachiwii.oink.ui.Tab
import com.hachiwii.oink.ui.UiInit
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** 退出设备组 at the end of settings (user 2026-11-11). */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class SettingsLeaveGroupTest {
    @get:Rule val compose = createComposeRule()

    private val windowsPc = Device("windowsPc", "Windows PC", true, computer = true, platform = "windows").toPeer(null)

    private fun settings(model: AppModel, actions: AppActions, left: () -> Unit = {}, closed: () -> Unit = {}) {
        compose.setContent {
            OinkTheme(OinkColors.of(false, Accent.Pink)) { SettingsPage(UiInit(), Prefs(scheme = "light"), {}, model, actions, onLeftGroup = left, onClose = closed) }
        }
        compose.waitForIdle()
    }

    private fun leave() {
        compose.onNodeWithText("退出设备组").performScrollTo().performClick()
        compose.waitForIdle()
        compose.onNodeWithText("退出后，需要重新配对才能和组内设备共享。").assertExists()
        // Manual clock from here: the toasts clear themselves after 2 s.
        compose.mainClock.autoAdvance = false
        compose.onNodeWithText("退出").performClick()
        compose.mainClock.advanceTimeBy(600)
    }

    @Test fun hiddenWhenNoOtherDeviceIsInTheGroup() {
        settings(AppModel(), AppActions())
        compose.onNodeWithText("开源许可").assertExists()
        compose.onNodeWithText("退出设备组").assertDoesNotExist()
    }

    @Test fun cancelKeepsTheGroup() {
        var calls = 0
        settings(AppModel(peers = listOf(windowsPc)), AppActions(leaveGroup = { calls++; null }))
        compose.onNodeWithText("退出设备组").performScrollTo().performClick()
        compose.waitForIdle()
        compose.onNodeWithText("取消").performClick()
        compose.waitForIdle()
        compose.onNodeWithText("退出后，需要重新配对才能和组内设备共享。").assertDoesNotExist()
        assertEquals(0, calls)
    }

    @Test fun confirmedLeaveClosesSettings() {
        var calls = 0; var left = 0; var closed = 0
        settings(AppModel(peers = listOf(windowsPc)), AppActions(leaveGroup = { calls++; null }), { left++ }, { closed++ })
        leave()
        assertEquals(listOf(1, 1, 1), listOf(calls, left, closed))
    }

    @Test fun failureShowsTheReasonAndStays() {
        var left = 0; var closed = 0
        settings(AppModel(peers = listOf(windowsPc)), AppActions(leaveGroup = { "无法离开设备组：网络不可用" }), { left++ }, { closed++ })
        leave()
        compose.onNodeWithText("无法离开设备组：网络不可用").assertExists()
        assertEquals(listOf(0, 0), listOf(left, closed))
    }

    @Test fun appToastsAndReturnsToTheDevicePage() {
        compose.setContent {
            OinkApp(AppModel(peers = listOf(windowsPc)), UiInit(tab = Tab.Transfer, settings = true), Prefs(scheme = "light"), {}, AppActions(leaveGroup = { null }), false)
        }
        compose.waitForIdle()
        leave()
        compose.onNodeWithText("已退出设备组").assertExists()
        compose.onNodeWithText("开源许可").assertDoesNotExist()
        // The device page shows its devices (the transfer page would not).
        compose.onNodeWithContentDescription("打开 Windows PC 设备控制").assertExists()
    }
}
