package com.hachiwii.oink

import androidx.compose.animation.core.Animatable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.unit.dp
import com.hachiwii.oink.ui.pressHold
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** The device sheet's press recognizer: scale feedback plus a tap that sees the current state. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class DeviceSheetTapTest {
    @get:Rule val compose = createComposeRule()

    private fun tap() {
        compose.onNodeWithTag("control").performTouchInput { down(center); advanceEventTime(60); up() }
        compose.waitForIdle()
    }

    @Test
    fun eachTapAnimatesAndChoosesTheOtherModeOfTheCurrentState() {
        val mode = mutableStateOf("shared")
        val chosen = mutableListOf<String>()
        compose.setContent {
            val scale = remember { Animatable(1f) }
            val scope = rememberCoroutineScope()
            // As RoundControl: the callback closes over the mode of this composition.
            val current = mode.value
            val tap by rememberUpdatedState { val next = if (current == "shared") "local" else "shared"; chosen += next; mode.value = next }
            Box(Modifier.size(80.dp).testTag("control").pressHold(true, true, scale, scope, { tap() }, {}))
        }
        // The press feedback animates the scale; with a bare MainScope that threw (no frame clock).
        tap()
        tap()
        assertEquals(listOf("local", "shared"), chosen)
    }
}
