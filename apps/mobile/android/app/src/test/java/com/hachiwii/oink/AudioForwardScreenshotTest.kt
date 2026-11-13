package com.hachiwii.oink

import android.app.Notification
import android.service.quicksettings.Tile
import android.view.ContextThemeWrapper
import android.view.View
import android.widget.LinearLayout
import android.widget.TextView
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.Modifier
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.wrapContentHeight
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.viewinterop.AndroidView
import com.github.takahirom.roborazzi.ExperimentalRoborazziApi
import com.github.takahirom.roborazzi.captureRoboImage
import com.github.takahirom.roborazzi.captureScreenRoboImage
import com.hachiwii.oink.control.KeyboardKind
import com.hachiwii.oink.control.ControlState
import com.hachiwii.oink.control.ControlTarget
import com.hachiwii.oink.tiles.*
import com.hachiwii.oink.ui.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.ParameterizedRobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** Native app surfaces in both themes. SystemUI cannot run in Robolectric: notification shots use
 * Android's actual expanded RemoteViews; tile shots are an explicitly test-only model/icon host. */
@RunWith(ParameterizedRobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class AudioForwardScreenshotTest(private val dark: Boolean) {
    companion object {
        @JvmStatic @ParameterizedRobolectricTestRunner.Parameters(name = "dark={0}")
        fun themes() = listOf(arrayOf<Any>(false), arrayOf<Any>(true))
    }
    @get:Rule val compose = createComposeRule()
    private val prefs = Prefs(scheme = if (dark) "dark" else "light", privileged = "on")
    private val model = AppModel(forwardOn = true, peers = listOf(
        PeerUi("pc", "Computer", "windows", speaker = "outlet", latency = 4.0),
        PeerUi("android", "Android", "android", speaker = "forward", mic = "send", latency = 12.0),
        PeerUi("ios", "iPhone", "iphone", speaker = "forward", latency = 8.0)))
    private fun path(name: String) = "build/outputs/roborazzi/audio-forward/${if (dark) "dark" else "light"}/$name.png"
    @OptIn(ExperimentalRoborazziApi::class)
    private fun shot(name: String, init: UiInit = UiInit(), model: AppModel = this.model, prefs: Prefs = this.prefs, popup: Boolean = false) {
        val control = FakeControl(ControlState(focusDevice = "pc", selectedDevice = "pc", targets = listOf(ControlTarget("pc", "Computer", "windows", true))))
        compose.setContent { OinkApp(model, init.copy(freeze = true), prefs, {}, AppActions(control = control), dark) }
        compose.waitForIdle()
        if (popup) captureScreenRoboImage(path(name)) else compose.onRoot().captureRoboImage(path(name))
    }
    @Test fun toolbarAndOtherPhones() = shot("devices")
    @Test fun failedToolbar() = shot("devices-failed", model = model.copy(forwardOn = false, forwardFailed = true))
    @Test fun disabledAdjust() = shot("control-adjust", UiInit(tab = Tab.Control, control = ControlInit(menu = "adjust")), popup = true)
    @Config(qualifiers = "w915dp-h412dp-land-xxhdpi")
    @Test fun disabledKeyboard() = shot("control-keyboard", UiInit(tab = Tab.Control, control = ControlInit(keyboard = KeyboardKind.EMULATED, expanded = true)))
    @Test fun privilegedAudioSettings() = shot("settings", UiInit(settings = true, settingsScroll = "audio"))
    @Test fun audioSettingsPicker() = shot("settings-picker", UiInit(settings = true, settingsScroll = "audio", settingsDialog = "picker:speakerTap"), popup = true)
    @Test fun hiddenAudioSettings() = shot("settings-nonprivileged", UiInit(settings = true, settingsScroll = "audio"), prefs = prefs.copy(privileged = "off", speakerTap = SpeakerTap.FORWARD))
    @Test fun toastVolume() = shot("toast-volume", UiInit(toast = ToastData("调节本机音量不影响转发")))
    @Test fun toastMuted() = shot("toast-muted", UiInit(toast = ToastData("转发中本机静音")))
    @Test fun toastRejected() = shot("toast-rejected", UiInit(toast = ToastData("转发音频仅在特权模式可用")), model = model.copy(forwardOn = false))
    @Test fun toastCaptureBusy() = shot("toast-busy", UiInit(toast = ToastData("转发音频失败：系统音频正被其它功能占用（如投屏）")), model = model.copy(forwardOn = false, forwardFailed = true))
    @Test fun toastSystemBlocked() = shot("toast-blocked", UiInit(toast = ToastData("请解锁手机后再开始转发")), model = model.copy(forwardOn = false, forwardFailed = true))
    private fun context(): ContextThemeWrapper {
        // RemoteViews resolves the posting app's context again, so its resources must also use
        // the test theme; an override on the host wrapper alone is lost during that lookup.
        RuntimeEnvironment.setQualifiers(if (dark) "+night" else "+notnight")
        val app = RuntimeEnvironment.getApplication()
        val config = android.content.res.Configuration(app.resources.configuration).apply {
            uiMode = (uiMode and android.content.res.Configuration.UI_MODE_NIGHT_MASK.inv()) or
                if (dark) android.content.res.Configuration.UI_MODE_NIGHT_YES else android.content.res.Configuration.UI_MODE_NIGHT_NO
        }
        return ContextThemeWrapper(app.createConfigurationContext(config), if (dark) android.R.style.Theme_Material else android.R.style.Theme_Material_Light)
    }
    @Test fun notification() {
        val context = context()
        val notice = PersistentNotice(NOTICE_TITLE_OPEN, noticeText(4, false, true, true), NoticeTap.OPEN, HomeTab.DEVICES, microphone = true, forward = true)
        val notification = PersistentNotices.build(context, notice)
        val remote = Notification.Builder.recoverBuilder(context, notification).createBigContentView()
        val view = remote.apply(context, LinearLayout(context))
        // SystemUI supplies the card surface; the framework RemoteViews leaves it transparent.
        view.setBackgroundColor(if (dark) 0xff202124.toInt() else 0xfffafafa.toInt())
        captureHost("notification-framework-template", view)
    }
    private fun tileHost(name: String, facts: TileFacts) {
        val context = context()
        val host = LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(48, 36, 48, 36)
            setBackgroundColor(if (dark) 0xff202124.toInt() else 0xfff3f3f5.toInt())
        }
        for (kind in listOf(TileKind.SPEAKER, TileKind.FORWARD)) {
            val look = tileLook(kind, facts, null)
            host.addView(TextView(context).apply {
                text = "${kind.label}  ${look.subtitle ?: if (look.state == Tile.STATE_ACTIVE) "开" else "关"}"
                textSize = 20f
                isEnabled = look.state != Tile.STATE_UNAVAILABLE
                alpha = if (isEnabled) 1f else .4f
                setTextColor(if (look.state == Tile.STATE_ACTIVE) 0xffd23878.toInt() else if (dark) 0xffeeeeee.toInt() else 0xff222222.toInt())
                setCompoundDrawablesWithIntrinsicBounds(if (kind == TileKind.FORWARD) R.drawable.ic_tile_forward else R.drawable.ic_tile_speaker, 0, 0, 0)
                compoundDrawablePadding = 24
                compoundDrawables[0]?.setTint(currentTextColor)
                setPadding(24, 28, 24, 28)
            })
        }
        captureHost(name, host)
    }
    private fun captureHost(name: String, view: View) {
        // Roborazzi needs an Activity-backed window; host the real Views in the test Activity.
        compose.setContent {
            AndroidView(factory = { view }, modifier = Modifier.fillMaxWidth().wrapContentHeight().testTag("audio-system-host"))
        }
        compose.waitForIdle()
        compose.onNodeWithTag("audio-system-host").captureRoboImage(path(name))
    }
    @Test fun tilesForwardingModelHost() = tileHost("tiles-forwarding-model-host", TileFacts(true, true, true, true, true, false, false, true, forwardOn = true, captureReady = true))
    @Test fun tilesNonPrivilegedModelHost() = tileHost("tiles-nonprivileged-model-host", TileFacts(true, true, true, true, false, false, false, true))
}
