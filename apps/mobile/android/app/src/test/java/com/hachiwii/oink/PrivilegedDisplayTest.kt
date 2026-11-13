package com.hachiwii.oink

import com.hachiwii.oink.recovery.PairingCodeState
import com.hachiwii.oink.recovery.PairingFailure
import com.hachiwii.oink.recovery.RecoveryPhase
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * 特权模式's page after a pairing (Phone, 2026-11-12): finished from the notification while Oink
 * was in the background, privileged mode came up but the page stayed on 设置中.
 */
class PrivilegedDisplayTest {
    private fun shown(override: String?, phase: RecoveryPhase, detail: String = "", enabled: Boolean = true) =
        override ?: privilegedFromRecovery(phase, detail, enabled)

    @Test fun aPairingFinishedFromTheNotificationLeavesSetup() {
        // The page was on 第 3 步 ("setup") when the user went to Settings; it comes back to Paired + Ready.
        val override = privilegedOverrideAfter(PairingCodeState.Paired(viaNotice = true), current = "setup")
        assertNull(override)
        assertEquals("on", shown(override, RecoveryPhase.Ready))
        // Still starting when the page comes back.
        assertEquals("starting", shown(override, RecoveryPhase.Pending, "waiting_for_network"))
        assertEquals("starting", shown(override, RecoveryPhase.Starting, "helper_attached"))
    }

    @Test fun pairingAndFailureAreShownByThePage() {
        assertEquals("starting", privilegedOverrideAfter(PairingCodeState.Pairing(viaNotice = false), "setup"))
        assertEquals("failed", privilegedOverrideAfter(PairingCodeState.Failed(PairingFailure.CODE_REJECTED, 41673, true), "setup"))
        // Idle (back on 第 3 步) keeps whatever the page chose.
        assertEquals("setup", privilegedOverrideAfter(PairingCodeState.Idle, "setup"))
        assertNull(privilegedOverrideAfter(PairingCodeState.Idle, null))
    }

    @Test fun recoveryStates() {
        assertEquals("on", privilegedFromRecovery(RecoveryPhase.Ready, "owner_ready", true))
        assertEquals("starting", privilegedFromRecovery(RecoveryPhase.Connecting, "localhost_adb", true))
        assertEquals("failed", privilegedFromRecovery(RecoveryPhase.Failed, "local_adb_failed", true))
        assertEquals("off", privilegedFromRecovery(RecoveryPhase.Failed, "local_adb_failed", false))
        // Enabled and waiting for the network after start(): 正在启动, not 未开启.
        assertEquals("starting", privilegedFromRecovery(RecoveryPhase.Pending, "waiting_for_network", true))
        assertEquals("off", privilegedFromRecovery(RecoveryPhase.Pending, "disabled", false))
        assertEquals("off", privilegedFromRecovery(RecoveryPhase.Pending, "idle", true))
    }
}
