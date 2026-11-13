package com.hachiwii.oink

import android.view.View
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.test.getBoundsInRoot
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import com.hachiwii.oink.ui.Accent
import com.hachiwii.oink.ui.BottomSheetLayer
import com.hachiwii.oink.ui.OinkColors
import com.hachiwii.oink.ui.OinkTheme
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** The activity is adjustNothing: bottom sheets must lift their content above the keyboard themselves. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class SheetImeTest {
    @get:Rule val compose = createComposeRule()
    private lateinit var view: View

    private fun show() {
        compose.setContent {
            view = LocalView.current
            OinkTheme(OinkColors.of(false, Accent.Pink)) {
                BottomSheetLayer(onDismiss = {}) { Box(Modifier.fillMaxWidth().height(48.dp).testTag("field")) }
            }
        }
        compose.waitForIdle()
    }

    private fun insets(nav: Int, ime: Int) {
        val insets = WindowInsetsCompat.Builder()
            .setInsets(WindowInsetsCompat.Type.navigationBars(), Insets.of(0, 0, 0, nav))
            .setInsets(WindowInsetsCompat.Type.ime(), Insets.of(0, 0, 0, ime))
            .setVisible(WindowInsetsCompat.Type.ime(), ime > 0)
            .build()
        compose.runOnUiThread { ViewCompat.dispatchApplyWindowInsets(view, insets) }
        compose.waitForIdle()
    }

    /** Distance from the sheet content's bottom to the screen's bottom. */
    private fun gap(): Dp = compose.onRoot().getBoundsInRoot().bottom - compose.onNodeWithTag("field", useUnmergedTree = true).getBoundsInRoot().bottom

    private fun px(value: Int): Dp = with(compose.density) { value.toDp() }

    @Test
    fun withoutKeyboardTheContentSitsAboveTheNavigationBar() {
        show()
        insets(nav = 48, ime = 0)
        assertEquals(px(48).value, gap().value, 0.5f)
    }

    @Test
    fun keyboardLiftsTheContentByItsInsetNotOnTopOfTheNavigationBar() {
        show()
        insets(nav = 48, ime = 600)
        assertEquals(px(600).value, gap().value, 0.5f)
        // The sheet follows the keyboard back down.
        insets(nav = 48, ime = 0)
        assertEquals(px(48).value, gap().value, 0.5f)
    }
}
