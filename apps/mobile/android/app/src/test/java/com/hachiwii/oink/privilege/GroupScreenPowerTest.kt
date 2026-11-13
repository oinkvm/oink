package com.hachiwii.oink.privilege

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.os.SystemClock
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowPowerManager
import java.time.Duration

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class GroupScreenPowerTest {
    @Test fun expiresWithoutAnAppCallbackAndDoesNotReviveStalePulses() {
        val power = RuntimeEnvironment.getApplication().getSystemService(PowerManager::class.java)
        shadowOf(power).setIsInteractive(true)
        val lease = GroupScreenPower(power, Handler(Looper.getMainLooper()))
        try {
            lease.activity(SystemClock.elapsedRealtime() + 3000)
            val lock = ShadowPowerManager.getLatestWakeLock()
            assertTrue(lock.isHeld)
            shadowOf(Looper.getMainLooper()).idleFor(Duration.ofMillis(2999))
            assertTrue(lock.isHeld)
            shadowOf(Looper.getMainLooper()).idleFor(Duration.ofMillis(1))
            assertFalse(lock.isHeld)
            lease.activity(SystemClock.elapsedRealtime() - 1)
            assertFalse(lock.isHeld)
        } finally { lease.close() }
    }

    @Test fun explicitScreenOffWinsAndGroupInputNeverWakesIt() {
        val power = RuntimeEnvironment.getApplication().getSystemService(PowerManager::class.java)
        shadowOf(power).setIsInteractive(true)
        val lease = GroupScreenPower(power, Handler(Looper.getMainLooper()))
        try {
            lease.activity(SystemClock.elapsedRealtime() + 3000)
            val lock = ShadowPowerManager.getLatestWakeLock()
            assertTrue(lock.isHeld)
            shadowOf(power).setIsInteractive(false)
            lease.displayChanged()
            assertFalse(lock.isHeld)
            lease.activity(SystemClock.elapsedRealtime() + 3000)
            assertFalse(power.isInteractive)
            assertFalse(lock.isHeld)
            shadowOf(power).setIsInteractive(true)
            lease.displayChanged()
            assertTrue(lock.isHeld)
            lease.activity(0)
            assertFalse(lock.isHeld)
        } finally { lease.close() }
    }
}
