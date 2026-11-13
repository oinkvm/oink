package com.hachiwii.oink

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.performClick
import com.hachiwii.oink.ui.Accent
import com.hachiwii.oink.ui.AppActions
import com.hachiwii.oink.ui.AppModel
import com.hachiwii.oink.ui.NearbyUi
import com.hachiwii.oink.ui.OinkApp
import com.hachiwii.oink.ui.OinkColors
import com.hachiwii.oink.ui.OinkTheme
import com.hachiwii.oink.ui.PairingLayer
import com.hachiwii.oink.ui.PairingView
import com.hachiwii.oink.ui.Prefs
import com.hachiwii.oink.ui.Tab
import com.hachiwii.oink.ui.ToastData
import com.hachiwii.oink.ui.UiInit
import com.hachiwii.oink.ui.UiState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** What follows a completed pairing: back to the device page, no device sheet (user 2026-11-11). */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class PairingCompletionTest {
    @get:Rule val compose = createComposeRule()

    private val peer = NearbyUi("studio", "Studio-Mac", "mac", "192.168.1.42", 47470, "7KQ4-M2PA-9DXF")

    /** As OinkScreen: the live session sheet, replaced by the success hold once the core reports completion. */
    private fun completes(live: PairingView) {
        val ui = UiState(UiInit(tab = Tab.Transfer, addPage = true))
        var session by mutableStateOf<PairingView?>(live)
        var notice by mutableStateOf<PairingNotice?>(null)
        compose.mainClock.autoAdvance = false
        compose.setContent {
            OinkTheme(OinkColors.of(false, Accent.Pink)) {
                val success = rememberCompletedPairing(notice, takeShown = { live }, clearNotice = { notice = null }, toast = {})
                PairingLayer(AppModel(pairing = success ?: session), UiInit(), ui, AppActions(), remember { mutableStateOf<ToastData?>(null) })
            }
        }
        compose.mainClock.advanceTimeBy(500)
        compose.runOnUiThread { session = null; notice = PairingNotice.Completed(PairPeer("studio", "Studio-Mac", "macos")) }
        compose.mainClock.advanceTimeBy(300)
        // The success sheet is still up: nothing moves underneath it yet.
        assertTrue(ui.addPage)
        assertEquals(Tab.Transfer, ui.tab)
        compose.mainClock.advanceTimeBy(PAIRING_SUCCESS_MS + 500)
        assertFalse(ui.addPage)
        assertEquals(Tab.Devices, ui.tab)
        assertEquals(null, ui.sheetPeer)
    }

    @Test fun initiatedHereReturnsToTheDevicePage() = completes(PairingView(peer, "out", "code", code = "482913"))

    @Test fun acceptedRequestReturnsToTheDevicePage() = completes(PairingView(peer, "in", "verifying", input = "246810"))

    @Test fun aFailureClosedByTheUserStaysOnItsPage() {
        val ui = UiState(UiInit(tab = Tab.Devices, addPage = true))
        var pairing by mutableStateOf<PairingView?>(PairingView(peer, "out", "failed", reason = "连接超时"))
        compose.setContent {
            OinkTheme(OinkColors.of(false, Accent.Pink)) {
                PairingLayer(AppModel(pairing = pairing), UiInit(), ui, AppActions(), remember { mutableStateOf<ToastData?>(null) })
            }
        }
        compose.waitForIdle()
        compose.runOnUiThread { pairing = null }
        compose.waitForIdle()
        assertTrue(ui.addPage)
    }

    /**
     * A kicked device leaves the model before its sheet's close animation ends; pairing it again
     * must not bring its control sheet back.
     */
    @Test fun aDeviceThatLeftWithItsSheetOpenDoesNotReopenItWhenPairedAgain() {
        val windowsPc = Device("windowsPc", "Windows PC", true, computer = true, platform = "windows").toPeer(null)
        var model by mutableStateOf(AppModel(peers = listOf(windowsPc)))
        compose.setContent { OinkApp(model, UiInit(), Prefs(scheme = "light"), {}, AppActions(), false) }
        compose.onNodeWithContentDescription("打开 Windows PC 设备控制").performClick()
        compose.waitForIdle()
        compose.onNodeWithContentDescription("踢出设备").assertExists()
        compose.runOnUiThread { model = model.copy(peers = emptyList()) }
        compose.waitForIdle()
        compose.runOnUiThread { model = model.copy(peers = listOf(windowsPc)) }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("踢出设备").assertDoesNotExist()
    }
}
