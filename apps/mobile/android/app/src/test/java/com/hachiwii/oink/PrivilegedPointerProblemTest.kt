package com.hachiwii.oink

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import com.hachiwii.oink.ui.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** 特权模式 on, but the helper reported that PCs cannot move the pointer: the page says so with the reason. */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class PrivilegedPointerProblemTest {
    @get:Rule val compose = createComposeRule()

    private fun page(problem: String?) {
        compose.setContent {
            OinkTheme(OinkColors.of(false, Accent.Pink)) {
                SettingsPage(UiInit(settingsStack = listOf("main", "privileged")), Prefs(scheme = "light", privileged = "on"), {},
                    AppModel(privilegedPointerProblem = problem), AppActions(), onClose = {})
            }
        }
        compose.waitForIdle()
    }

    @Test fun showsThePointerFailure() {
        page("IllegalStateException: pointer association: cmd companiondevice associate exit 255")
        compose.onNodeWithText("剪贴板可以自动读取并广播，但电脑暂时不能用鼠标操控本机：IllegalStateException: pointer association: cmd companiondevice associate exit 255").assertExists()
    }

    @Test fun normalTextWhenThePointerWorks() {
        page(null)
        compose.onNodeWithText("电脑可以用键盘鼠标操控本机，剪贴板可以自动读取并广播。").assertExists()
    }
}
