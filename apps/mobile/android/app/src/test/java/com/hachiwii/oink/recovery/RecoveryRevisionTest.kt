package com.hachiwii.oink.recovery

import org.junit.Assert.*
import org.junit.Test

class RecoveryRevisionTest {
    @Test fun leaveThenPairUsesRetainedHighWater() {
        assertEquals(10uL, RecoveryRevision.next(0u, 9u))
    }
    @Test fun syncedNewerRevisionWinsOverLocalHighWater() {
        assertEquals(13uL, RecoveryRevision.next(12u, 9u))
    }
    @Test fun enableAndDisableEachAdvanceIntent() {
        val enabled = RecoveryRevision.next(0u)
        val disabled = RecoveryRevision.next(enabled)
        val reenabled = RecoveryRevision.next(disabled)
        assertEquals(3uL, reenabled)
    }
    @Test fun stalePcDisableCannotOverwriteNewerEnable() {
        val observed = RecoveryIntent(true, "3")
        val newer = observed.updated(false).updated(true)
        assertNull(newer.disableIfRevision(observed.revision))
        assertTrue(newer.enabled)
        assertEquals("5", newer.revision)
    }
    @Test fun matchingDisableAdvancesAndCannotBeReplayed() {
        val observed = RecoveryIntent(true, "3")
        val disabled = requireNotNull(observed.disableIfRevision("3"))
        assertFalse(disabled.enabled)
        assertEquals("4", disabled.revision)
        assertNull(disabled.disableIfRevision("3"))
    }
    @Test(expected = IllegalStateException::class) fun exhaustedCounterNeverWraps() {
        RecoveryRevision.next(0u, ULong.MAX_VALUE)
    }
}
