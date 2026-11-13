package com.hachiwii.oink.privilege

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertThrows
import org.junit.Test

/**
 * The helper's UHID keyboard report (2026-11-13): the App always sends six key slots and an empty
 * slot is 0. Refusing 0 failed the first key report after the pointer entered the phone, which
 * turned privileged input off.
 */
class UhidKeyboardReportTest {
    private fun bytes(vararg values: Int) = ByteArray(values.size) { values[it].toByte() }

    @Test fun emptySlotsAreAccepted() {
        // Ctrl held, nothing else (the hotkey's modifier still down when focus enters the phone).
        assertArrayEquals(bytes(0x01, 0, 0, 0, 0, 0, 0, 0),
            UhidDevices.keyboardReport(0x01, bytes(0, 0, 0, 0, 0, 0)))
        // "a" in the first slot.
        assertArrayEquals(bytes(0, 0, 0x04, 0, 0, 0, 0, 0),
            UhidDevices.keyboardReport(0, bytes(0x04, 0, 0, 0, 0, 0)))
    }

    @Test fun errorUsagesAndOversizedReportsAreRefused() {
        assertThrows(IllegalArgumentException::class.java) { UhidDevices.keyboardReport(0, bytes(0x01, 0, 0, 0, 0, 0)) }
        assertThrows(IllegalArgumentException::class.java) { UhidDevices.keyboardReport(0, bytes(0xE0, 0, 0, 0, 0, 0)) }
        assertThrows(IllegalArgumentException::class.java) { UhidDevices.keyboardReport(256, bytes(0, 0, 0, 0, 0, 0)) }
        assertThrows(IllegalArgumentException::class.java) { UhidDevices.keyboardReport(0, ByteArray(7)) }
    }
}
