package com.hachiwii.oink

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import com.github.takahirom.roborazzi.captureRoboImage
import com.hachiwii.oink.ui.Accent
import com.hachiwii.oink.ui.OinkColors
import com.hachiwii.oink.ui.OinkTheme
import com.hachiwii.oink.ui.ShareCardLayer
import com.hachiwii.oink.ui.ShareCardUi
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.ParameterizedRobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The share card's states for the share-anim SPEC (2026-11-11), light and dark: waiting for a computer
 * or a phone, the hand-over at 230 ms, a phone target sending and done. Writes PNGs to
 * build/outputs/roborazzi/share-card; asserts nothing about pixels.
 */
@RunWith(ParameterizedRobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "zh-rCN-w390dp-h844dp-xxhdpi", application = android.app.Application::class)
class ShareCardShotsTest(private val dark: Boolean) {
    companion object {
        @JvmStatic @ParameterizedRobolectricTestRunner.Parameters(name = "dark={0}")
        fun themes() = listOf(arrayOf<Any>(false), arrayOf<Any>(true))
    }

    @get:Rule val compose = createComposeRule()

    private fun shot(name: String, card: ShareCardUi, reduceMotion: Boolean = false) {
        compose.setContent {
            OinkTheme(OinkColors.of(dark, Accent.Pink)) {
                ShareCardLayer(card, host = true, onHide = {}, onCancel = {}, reduceMotion = reduceMotion)
            }
        }
        compose.waitForIdle()
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/share-card/$name-${if (dark) "dark" else "light"}.png")
    }

    // State names as in native-rewrite.md 「分享卡片」.
    @Test fun waitComputer() = shot("share-wait", ShareCardUi("waiting"))
    @Test fun waitPhone() = shot("share-wait-phone", ShareCardUi("waiting", to = "iPhone"))
    @Test fun handoffComputer230() = shot("share-wait-online", ShareCardUi("indeterminate", freezeHandoff = 230f))
    @Test fun handoffPhone230() = shot("share-wait-online-phone", ShareCardUi("indeterminate", to = "iPhone", freezeHandoff = 230f))
    @Test fun phoneSending() = shot("share-phone-sending", ShareCardUi("sending", 52, 2_470_000.0, freezeT = .30f, to = "iPhone"))
    @Test fun phoneSendingReducedMotion() = shot("share-phone-reduced-motion", ShareCardUi("sending", 52, 2_470_000.0, to = "iPhone"), reduceMotion = true)
    @Test fun phoneDone() = shot("share-phone-done", ShareCardUi("done", 100, elapsed = 6.42, freezeT = .30f, to = "iPhone"))
}
