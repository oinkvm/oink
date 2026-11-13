package com.hachiwii.oink.recovery

import org.junit.Assert.*
import org.junit.Test

class RecoveryRebootHintTest {
    @Test fun onlyAndroid11To12NeedManualWirelessDebuggingAfterReboot() {
        for (api in 30..32) assertTrue(RecoveryPolicy.rebootNeedsWirelessDebugging(api))
        for (api in 33..36) assertFalse(RecoveryPolicy.rebootNeedsWirelessDebugging(api))
    }
}
