package com.hachiwii.oink.recovery

import org.junit.Assert.*
import org.junit.Test

class RecoveryPolicyTest {
    @Test fun olderAndroidNeverClaimsBootSupport() {
        for (api in 30..32) {
            assertFalse(RecoveryPolicy.bootAllowed(api, 0))
            assertFalse(RecoveryPolicy.canWriteSettings(api, true))
            assertFalse(RecoveryPolicy.needsPermission(false, api, false))
        }
    }
    @Test fun bootOnlyRunsForPrimaryUser() {
        assertTrue(RecoveryPolicy.bootAllowed(33, 0))
        assertTrue(RecoveryPolicy.bootAllowed(35, 0))
        assertFalse(RecoveryPolicy.bootAllowed(35, 10))
    }
    @Test fun manualLocalAdbDoesNotRequireSecureSettings() {
        assertFalse(RecoveryPolicy.needsPermission(false, 35, false))
        assertTrue(RecoveryPolicy.needsPermission(true, 35, false))
        assertFalse(RecoveryPolicy.needsPermission(true, 35, true))
        assertFalse(RecoveryPolicy.canWriteSettings(35, false))
    }
}
