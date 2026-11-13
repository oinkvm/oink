package com.hachiwii.oink

import com.hachiwii.oink.clipboard.NativeClipboardWrite
import com.hachiwii.oink.clipboard.nativeWriteResult
import com.hachiwii.oink.input.HelperReply
import com.hachiwii.oink.input.helperReply
import com.hachiwii.oink.recovery.RecoveryPolicy
import org.junit.Assert.*
import org.junit.Test

/** Host decisions behind the 0.4.0 review fixes (Android host block). */
class HostReviewFixesTest {
    private fun pc(id: String, online: Boolean, connection: DeviceConnection? = if (online) DeviceConnection.ONLINE else DeviceConnection.DISCONNECTED) =
        Device(id, "PC $id", online, computer = true, connection = connection)
    private fun phone(id: String, online: Boolean, connection: DeviceConnection? = if (online) DeviceConnection.ONLINE else DeviceConnection.DISCONNECTED) =
        Device(id, "Phone $id", online, computer = false, connection = connection)
    private val self = Device("me", "me", true, isSelf = true, connection = DeviceConnection.THIS_DEVICE)
    private val text = ShareDraft("hello")
    private val file = ShareDraft(items = listOf(LocalItem("/staged/a.zip", "a.zip", "application/zip", 10)))

    // L18 / DECISIONS 13: text and small images reach the other phones without any computer.
    @Test fun textWithoutComputersIsBroadcastToOnlinePhones() {
        val route = sendRoute(true, listOf(self, pc("p", false), phone("m", true)), null, text)
        assertEquals(SendRoute.Ready(null, "Phone m"), route)
        assertEquals(SendRoute.Fail("没有在线的设备"), sendRoute(true, listOf(self, pc("p", false), phone("m", false)), null, text))
        // A chosen proxy does not narrow a broadcast.
        assertEquals(null, (sendRoute(true, listOf(self, pc("p", true), phone("m", true)), "p", text) as SendRoute.Ready).proxy)
    }

    @Test fun proxiedContentStillNeedsAnOnlineComputer() {
        assertEquals(SendRoute.Fail("没有在线的电脑"), sendRoute(true, listOf(self, phone("m", true), pc("p", false)), "p", file))
        assertEquals(SendRoute.Ready("p", "PC p"), sendRoute(true, listOf(self, pc("p", true)), "p", file))
        assertEquals(SendRoute.Fail("此内容请选一台电脑"), sendRoute(true, listOf(self, pc("p", true)), null, file))
        // The system share entry picks the first online computer itself.
        assertEquals(SendRoute.Ready("q", "PC q"), sendRoute(true, listOf(self, pc("p", false), pc("q", true)), null, file, autoProxy = true))
    }

    // H18: a staged send waits for the core and its target as events instead of failing at once.
    @Test fun sendWaitsForCoreAndConnectingTarget() {
        assertEquals(SendRoute.Wait, sendRoute(false, emptyList(), "p", file))
        assertEquals(SendRoute.Wait, sendRoute(true, listOf(self, pc("p", false, DeviceConnection.CONNECTING)), "p", file))
        // A restarted core has not reported link state yet.
        assertEquals(SendRoute.Wait, sendRoute(true, listOf(self, pc("p", false, connection = null)), "p", file))
        assertEquals(SendRoute.Wait, sendRoute(true, listOf(self, phone("m", false, DeviceConnection.CONNECTING)), null, text))
        // A core that failed to start is a definitive answer (M40).
        assertEquals(SendRoute.Fail("连接服务没能启动：broken state"), sendRoute(false, emptyList(), "p", file, startupFailure = "broken state"))
    }

    @Test fun pickerAndRunningSendKeepTheServiceWithoutPersistentNotification() {
        assertTrue(shouldStopService(0, false, persistentNotification = false, picking = false, sending = false))
        assertFalse(shouldStopService(0, false, persistentNotification = false, picking = true, sending = false))
        assertFalse(shouldStopService(0, false, persistentNotification = false, picking = false, sending = true))
        assertFalse(shouldStopService(0, true, persistentNotification = false, picking = false, sending = false))
        assertFalse(shouldStopService(1, false, persistentNotification = false, picking = false, sending = false))
        assertFalse(shouldStopService(0, false, persistentNotification = true, picking = false, sending = false))
    }

    // M41: "开机后自动恢复" decides reboots only.
    @Test fun bootRecoveryFollowsTheAutoRecoverSwitch() {
        val boot = android.content.Intent.ACTION_BOOT_COMPLETED
        val locked = android.content.Intent.ACTION_LOCKED_BOOT_COMPLETED
        val update = android.content.Intent.ACTION_MY_PACKAGE_REPLACED
        assertTrue(RecoveryPolicy.restoreOnBroadcast(boot, enabled = true, autoRecover = true))
        assertFalse(RecoveryPolicy.restoreOnBroadcast(boot, enabled = true, autoRecover = false))
        assertFalse(RecoveryPolicy.restoreOnBroadcast(locked, enabled = true, autoRecover = false))
        assertFalse(RecoveryPolicy.restoreOnBroadcast(boot, enabled = false, autoRecover = true))
        assertTrue(RecoveryPolicy.restoreOnBroadcast(update, enabled = true, autoRecover = false))
        assertFalse(RecoveryPolicy.restoreOnBroadcast("other", enabled = true, autoRecover = true))
    }

    // M38: device moments fail one command; only a broken helper is fatal.
    @Test fun lockedOrStaleHelperRepliesAreNotFatal() {
        for (status in listOf("locked", "stale_display", "pointer_unavailable", "outside_display", "owner_inactive")) {
            assertEquals(status, HelperReply.UNAVAILABLE, helperReply(status))
        }
        for (status in listOf("ok", "pointer", "keyboard_pending")) assertEquals(HelperReply.OK, helperReply(status))
        for (status in listOf("failed", "permission_denied", "unsupported", null)) assertEquals(HelperReply.FATAL, helperReply(status))
    }

    // M42: a locked helper hands the write to the App instead of dropping the received text.
    @Test fun lockedHelperClipboardWriteFallsBackToTheApp() {
        assertEquals(NativeClipboardWrite.Unavailable, nativeWriteResult("locked"))
        assertEquals(NativeClipboardWrite.Unavailable, nativeWriteResult("owner_inactive"))
        assertNull(nativeWriteResult("written"))
        assertEquals(NativeClipboardWrite.Stale, nativeWriteResult("stale"))
    }
}
