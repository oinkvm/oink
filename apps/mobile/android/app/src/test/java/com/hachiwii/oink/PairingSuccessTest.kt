package com.hachiwii.oink

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.junit4.createComposeRule
import com.hachiwii.oink.ui.NearbyUi
import com.hachiwii.oink.ui.PairingView
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class PairingSuccessTest {
    @get:Rule val compose = createComposeRule()

    @Test
    fun successSheetClosesAfterTheNoticeIsCleared() {
        // Like the repository: clearing the notice is a state change that recomposes the screen.
        var notice by mutableStateOf<PairingNotice?>(null)
        val shown = mutableListOf<String?>()
        val toasts = mutableListOf<String>()
        val sheet = PairingView(NearbyUi("id", "Mac", "mac", "192.168.1.2", 47470), "out", "code", code = "000000")
        compose.setContent {
            val view = rememberCompletedPairing(notice, takeShown = { sheet }, clearNotice = { notice = null }, toast = { toasts += it })
            if (shown.lastOrNull() != view?.status || shown.isEmpty()) shown += view?.status
        }
        compose.runOnUiThread { notice = PairingNotice.Completed(PairPeer("id", "Mac", "macos")) }
        compose.waitForIdle()
        compose.mainClock.advanceTimeBy(PAIRING_SUCCESS_MS + 500)
        compose.waitForIdle()
        assertEquals(listOf(null, "success", null), shown)
        assertNull(notice)
        assertEquals(listOf("已与 Mac 配对"), toasts)
    }
}
