package com.hachiwii.oink

import android.content.pm.PackageManager
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.LifecycleRegistry
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import org.robolectric.RuntimeEnvironment
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class LowLatencyWifiTest {
    private val context get() = RuntimeEnvironment.getApplication()

    private class Screen : LifecycleOwner {
        val registry = LifecycleRegistry.createUnsafe(this)
        override val lifecycle: Lifecycle get() = registry
    }

    private fun LowLatencyWifi.assertHeld(lowLatency: Boolean, highPerformance: Boolean) {
        assertEquals("low latency", lowLatency, lowLatencyHeld)
        assertEquals("high performance", highPerformance, highPerformanceHeld)
    }

    private fun audio(speaker: Boolean = false, microphone: Boolean = false, joined: Boolean = true) =
        AppState(localAudio = LocalAudio(speakerOn = speaker, microphoneOn = microphone, joined = joined))

    @Test
    fun planFollowsScreensAndAudioRole() {
        assertEquals(WifiLockPlan(lowLatency = false, highPerformance = false), wifiLockPlan(0, audioRole = false))
        assertEquals(WifiLockPlan(lowLatency = true, highPerformance = false), wifiLockPlan(1, audioRole = false))
        assertEquals(WifiLockPlan(lowLatency = true, highPerformance = true), wifiLockPlan(0, audioRole = true))
        assertEquals(WifiLockPlan(lowLatency = true, highPerformance = true), wifiLockPlan(2, audioRole = true))
    }

    @Test
    fun audioRoleNeedsASwitchAndTheJoinedGroupAudio() {
        assertFalse(LocalAudio().audioRole)
        assertTrue(LocalAudio(speakerOn = true, joined = true).audioRole)
        assertTrue(LocalAudio(microphoneOn = true, joined = true).audioRole)
        // Switch on but still waiting for the group's audio configuration: no role yet.
        assertFalse(LocalAudio(speakerOn = true, microphoneOn = true, joined = false).audioRole)
        assertFalse(LocalAudio(joined = true).audioRole)
    }

    @Test
    fun screenAloneHoldsOnlyTheLowLatencyLockWhileStarted() {
        val wifi = LowLatencyWifi(context)
        val screen = Screen()
        screen.registry.addObserver(wifi.screen)
        screen.registry.currentState = Lifecycle.State.CREATED
        wifi.assertHeld(lowLatency = false, highPerformance = false)
        screen.registry.currentState = Lifecycle.State.RESUMED
        wifi.assertHeld(lowLatency = true, highPerformance = false)
        // Background (stopped): released, never kept.
        screen.registry.currentState = Lifecycle.State.CREATED
        wifi.assertHeld(lowLatency = false, highPerformance = false)
        // Back to foreground, then destroyed.
        screen.registry.currentState = Lifecycle.State.STARTED
        wifi.assertHeld(lowLatency = true, highPerformance = false)
        screen.registry.currentState = Lifecycle.State.DESTROYED
        wifi.assertHeld(lowLatency = false, highPerformance = false)
    }

    @Test
    fun twoScreensKeepTheLockUntilTheLastStops() {
        val wifi = LowLatencyWifi(context)
        val first = Screen().also { it.registry.addObserver(wifi.screen) }
        val second = Screen().also { it.registry.addObserver(wifi.screen) }
        first.registry.currentState = Lifecycle.State.STARTED
        second.registry.currentState = Lifecycle.State.STARTED
        first.registry.currentState = Lifecycle.State.CREATED
        wifi.assertHeld(lowLatency = true, highPerformance = false)
        second.registry.currentState = Lifecycle.State.CREATED
        wifi.assertHeld(lowLatency = false, highPerformance = false)
    }

    @Test
    fun audioRoleHoldsBothLocksWithOrWithoutAScreen() {
        val wifi = LowLatencyWifi(context)
        val screen = Screen().also { it.registry.addObserver(wifi.screen) }
        // Role with the screen off / Oink in the background.
        wifi.audioRole(true)
        wifi.assertHeld(lowLatency = true, highPerformance = true)
        // Screen comes and goes: the role keeps both.
        screen.registry.currentState = Lifecycle.State.RESUMED
        wifi.assertHeld(lowLatency = true, highPerformance = true)
        screen.registry.currentState = Lifecycle.State.CREATED
        wifi.assertHeld(lowLatency = true, highPerformance = true)
        // Role ends on screen: only the screen's lock stays.
        screen.registry.currentState = Lifecycle.State.STARTED
        wifi.audioRole(false)
        wifi.assertHeld(lowLatency = true, highPerformance = false)
        screen.registry.currentState = Lifecycle.State.CREATED
        wifi.assertHeld(lowLatency = false, highPerformance = false)
    }

    @Test
    fun theServiceFollowsTheRoleAndReleasesWhenItStops() = runBlocking {
        val wifi = LowLatencyWifi(context)
        val screen = Screen().also { it.registry.addObserver(wifi.screen) }
        val state = MutableStateFlow(AppState())
        // Unconfined: every state change is applied before the next line, on this (main) thread.
        val service = CoroutineScope(Dispatchers.Unconfined).launch { followAudioRole(state, wifi) }
        wifi.assertHeld(lowLatency = false, highPerformance = false)
        state.value = audio(speaker = true)
        wifi.assertHeld(lowLatency = true, highPerformance = true)
        state.value = audio(speaker = true, microphone = true)
        wifi.assertHeld(lowLatency = true, highPerformance = true)
        state.value = audio()
        wifi.assertHeld(lowLatency = false, highPerformance = false)
        state.value = audio(microphone = true)
        wifi.assertHeld(lowLatency = true, highPerformance = true)
        // A group switch: switch still on, not joined yet.
        state.value = audio(microphone = true, joined = false)
        wifi.assertHeld(lowLatency = false, highPerformance = false)
        state.value = audio(microphone = true)
        screen.registry.currentState = Lifecycle.State.STARTED
        // The service stops while the role is held and a screen is visible: the screen keeps its lock.
        service.cancelAndJoin()
        wifi.assertHeld(lowLatency = true, highPerformance = false)
        screen.registry.currentState = Lifecycle.State.CREATED
        wifi.assertHeld(lowLatency = false, highPerformance = false)
    }

    @Test
    fun manifestRequestsWakeLockForTheWifiLock() {
        @Suppress("DEPRECATION")
        val requested = context.packageManager.getPackageInfo(context.packageName, PackageManager.GET_PERMISSIONS).requestedPermissions.orEmpty()
        assertTrue(android.Manifest.permission.WAKE_LOCK in requested)
    }
}
