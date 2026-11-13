package com.hachiwii.oink

import android.content.ContentResolver
import android.database.ContentObserver
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.compose.LifecycleResumeEffect

/**
 * The system switches 特权模式's guide checks off: 「开发者选项」 and 「无线调试」, as the system has them
 * now. [wirelessDebugging] is null when it cannot be read (the guide then keeps its own step).
 */
data class DebugSwitches(val developerOptions: Boolean, val wirelessDebugging: Boolean?)

/** Settings.Global.ADB_WIFI_ENABLED is hidden API; the system writes it when 「无线调试」 is switched. */
internal const val ADB_WIFI_ENABLED = "adb_wifi_enabled"

internal fun readDebugSwitches(resolver: ContentResolver) = DebugSwitches(
    developerOptions = Settings.Global.getInt(resolver, Settings.Global.DEVELOPMENT_SETTINGS_ENABLED, 0) == 1,
    wirelessDebugging = runCatching { Settings.Global.getInt(resolver, ADB_WIFI_ENABLED, 0) == 1 }.getOrNull(),
)

/**
 * Live [DebugSwitches]. Wireless debugging goes off on its own (every reboot, and when Wi‑Fi drops),
 * so the guide must never trust a step it saw done earlier: the value follows the settings while
 * Oink is open (also in split screen next to Settings) and is read again whenever Oink comes back.
 */
@Composable
internal fun rememberDebugSwitches(): DebugSwitches {
    val resolver = LocalContext.current.contentResolver
    var value by remember(resolver) { mutableStateOf(readDebugSwitches(resolver)) }
    DisposableEffect(resolver) {
        val observer = object : ContentObserver(Handler(Looper.getMainLooper())) {
            override fun onChange(selfChange: Boolean) { value = readDebugSwitches(resolver) }
        }
        for (key in listOf(Settings.Global.DEVELOPMENT_SETTINGS_ENABLED, ADB_WIFI_ENABLED)) {
            resolver.registerContentObserver(Settings.Global.getUriFor(key), false, observer)
        }
        onDispose { resolver.unregisterContentObserver(observer) }
    }
    LifecycleResumeEffect(resolver) {
        value = readDebugSwitches(resolver)
        onPauseOrDispose {}
    }
    return value
}
