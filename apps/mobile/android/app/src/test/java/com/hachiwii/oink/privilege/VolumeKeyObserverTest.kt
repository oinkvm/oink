package com.hachiwii.oink.privilege

import java.io.BufferedReader
import java.io.StringReader
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35], application = android.app.Application::class)
class VolumeKeyObserverTest {
    @Test fun discoversCapabilitiesAcrossDevicesWithoutNamesOrFixedNodes() {
        val capabilities = """
            add device 1: /dev/input/event9
              name: "arbitrary A"
              events: KEY (0001): KEY_POWER
            add device 2: /dev/input/event12
              name: "arbitrary B"
              events: KEY (0001): KEY_VOLUMEUP KEY_VOLUMEDOWN
            add device 3: /dev/input/event4
              events: KEY (0001): KEY_VOLUMEDOWN
        """.trimIndent()
        assertEquals(listOf("/dev/input/event12", "/dev/input/event4"),
            VolumeKeyObserver.volumeDevices(BufferedReader(StringReader(capabilities))))
    }
    @Test fun sendsHidDownUpAndRepeatWithoutForwardingOtherKeys() {
        val down = VolumeKeyObserver.keyEvent(1, 115, 1)!!
        val up = VolumeKeyObserver.keyEvent(1, 115, 0)!!
        assertEquals(0xe9, down.getInt("usage"))
        assertTrue(down.getBoolean("pressed"))
        assertFalse(up.getBoolean("pressed"))
        assertEquals(0xea, VolumeKeyObserver.keyEvent(1, 114, 1)!!.getInt("usage"))
        assertTrue(VolumeKeyObserver.keyEvent(1, 115, 2)!!.getBoolean("pressed"))
        assertNull(VolumeKeyObserver.keyEvent(1, 116, 1))
        assertNull(VolumeKeyObserver.keyEvent(0, 0, 0))
    }
}
