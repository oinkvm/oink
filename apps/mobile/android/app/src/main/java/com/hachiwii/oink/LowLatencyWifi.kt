package com.hachiwii.oink

import android.content.Context
import android.net.wifi.WifiManager
import androidx.annotation.MainThread
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map

/** The Wi-Fi locks Oink wants held: see [wifiLockPlan]. */
internal data class WifiLockPlan(val lowLatency: Boolean, val highPerformance: Boolean)

/**
 * Low latency while an Oink screen is visible (bug 13) or while this phone holds an audio role
 * (speaker exit or microphone sender, user 2026-11-11); high performance too while it holds an
 * audio role, the only lock that can still act with the screen off (see [LowLatencyWifi]).
 */
internal fun wifiLockPlan(visibleScreens: Int, audioRole: Boolean) =
    WifiLockPlan(lowLatency = visibleScreens > 0 || audioRole, highPerformance = audioRole)

/**
 * Oink's only owner of Wi-Fi locks (one per app, [OinkApplication.wifi]); main thread only.
 *
 * Without a lock the phone's Wi-Fi stays in 802.11 power save, which buffers incoming frames at the
 * access point until the next beacon/wake: measured Mac → phone ping 15.7 ms average with 17 ms
 * jitter on the same Wi-Fi where PC ↔ PC is 4.5 ms. Needs WAKE_LOCK.
 *
 * Two inputs, both plain state (no reference counting across callers):
 * - [screen]: every Oink screen observes it; started screens are counted. WIFI_MODE_FULL_LOW_LATENCY
 *   only acts while Oink is the foreground app with the screen on, so it costs nothing elsewhere.
 * - [audioRole]: set by the connection service while this phone holds the speaker-exit or
 *   microphone-sender role, false when the role ends and when the service stops.
 *
 * Why WIFI_MODE_FULL_HIGH_PERF as well (an OS workaround): the low-latency lock is ignored with the
 * screen off or Oink in the background, exactly when a phone playing the computer's sound or sharing
 * its microphone usually is. HIGH_PERF is the only public lock that keeps Wi-Fi out of power save
 * there ("active even when the device screen is off or the acquiring application is running in the
 * background"). It is deprecated in API 34, and from Android 14 the Wi-Fi stack converts it into a
 * low-latency lock unless the device config `high_perf_lock_deprecated` (default true) is off, so on
 * most Android 14+ phones it adds nothing with the screen off. It still does on Android 11-13 and
 * on devices with that config off, and holding both is what the platform documents for this case:
 * the low-latency lock while foreground with the screen on, the high-performance one otherwise.
 */
internal class LowLatencyWifi(context: Context) {
    private val manager = context.applicationContext.getSystemService(WifiManager::class.java)
    private val lowLatency = manager?.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "Oink:low-latency")
        ?.apply { setReferenceCounted(false) }
    @Suppress("DEPRECATION") // See the class comment: the only screen-off lock.
    private val highPerformance = manager?.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "Oink:audio-high-perf")
        ?.apply { setReferenceCounted(false) }
    private var visibleScreens = 0
    private var audioRole = false

    val lowLatencyHeld: Boolean get() = lowLatency?.isHeld == true
    val highPerformanceHeld: Boolean get() = highPerformance?.isHeld == true

    /** Add to every Oink screen's lifecycle: started (visible) screens hold the low-latency lock. */
    val screen: DefaultLifecycleObserver = object : DefaultLifecycleObserver {
        override fun onStart(owner: LifecycleOwner) {
            visibleScreens++
            apply()
        }

        override fun onStop(owner: LifecycleOwner) {
            visibleScreens = (visibleScreens - 1).coerceAtLeast(0)
            apply()
        }
    }

    /** This phone holds (or no longer holds) the speaker-exit or microphone-sender role. */
    @MainThread
    fun audioRole(held: Boolean) {
        audioRole = held
        apply()
    }

    private fun apply() {
        val plan = wifiLockPlan(visibleScreens, audioRole)
        hold(lowLatency, plan.lowLatency)
        hold(highPerformance, plan.highPerformance)
        android.util.Log.i("OinkWifi", "locks screens=$visibleScreens audio=$audioRole " +
            "low_latency=${lowLatency?.isHeld} high_perf=${highPerformance?.isHeld}")
    }

    private fun hold(lock: WifiManager.WifiLock?, wanted: Boolean) {
        if (lock == null || lock.isHeld == wanted) return
        if (wanted) lock.acquire() else lock.release()
    }
}

/**
 * The connection service's part of the Wi-Fi locks: follows [AppState.localAudio]'s audio role until
 * cancelled (the service stops), then releases it. Runs on the main thread.
 */
internal suspend fun followAudioRole(state: Flow<AppState>, wifi: LowLatencyWifi) {
    try {
        state.map { it.localAudio.audioRole }.distinctUntilChanged().collect { wifi.audioRole(it) }
    } finally {
        wifi.audioRole(false)
    }
}
