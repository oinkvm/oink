package com.hachiwii.oink

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import com.hachiwii.oink.ui.Accent
import com.hachiwii.oink.ui.AppActions
import com.hachiwii.oink.ui.DeviceSheet
import com.hachiwii.oink.ui.OinkColors
import com.hachiwii.oink.ui.OinkTheme
import com.hachiwii.oink.ui.PeerUi
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** Phones have no paused state: their device sheet offers only the kick button (user 2026-11-11). */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class DeviceSheetPauseTest {
    @get:Rule val compose = createComposeRule()

    private fun sheet(peer: PeerUi) {
        compose.setContent { OinkTheme(OinkColors.of(false, Accent.Pink)) { DeviceSheet(peer, null, AppActions()) {} } }
        compose.waitForIdle()
    }

    @Test fun phoneSheetHasOnlyTheKickButton() {
        sheet(Device("pixel", "Pixel 9", true, platform = "android").toPeer(null))
        compose.onNodeWithContentDescription("暂停").assertDoesNotExist()
        compose.onNodeWithContentDescription("踢出设备").assertExists()
    }

    @Test fun iphoneSheetHasOnlyTheKickButton() {
        sheet(Device("iphone", "iPhone", true, platform = "ios").toPeer(null))
        compose.onNodeWithContentDescription("暂停").assertDoesNotExist()
        compose.onNodeWithContentDescription("踢出设备").assertExists()
    }

    @Test fun computerSheetKeepsPause() {
        sheet(Device("windowsPc", "Windows PC", true, computer = true, platform = "windows").toPeer(null))
        compose.onNodeWithContentDescription("暂停").assertExists()
        compose.onNodeWithContentDescription("踢出设备").assertExists()
    }
}
