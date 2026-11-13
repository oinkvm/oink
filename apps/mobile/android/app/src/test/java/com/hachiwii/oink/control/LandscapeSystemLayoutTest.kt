package com.hachiwii.oink.control

import org.junit.Assert.*
import org.junit.Test

class LandscapeSystemLayoutTest {
    @Test fun fixedThresholdIncludes135() {
        assertEquals(405f, LandscapeSystemLayout.REFERENCE_WIDTH, .001f)
        assertFalse(LandscapeSystemLayout.split(117.45f))
        assertFalse(LandscapeSystemLayout.split(134f))
        assertTrue(LandscapeSystemLayout.split(135f))
        assertTrue(LandscapeSystemLayout.split(240f))
    }

    @Test fun twoRowsPutNavigationBeforeMediaAndKeepAll26Keys() {
        val rows = LandscapeSystemLayout.rows(134f, fn = false, mac = false)
        assertEquals(listOf("esc", "copy", "paste", "home", "end", "pgup", "pgdn", "prev", "play", "next", "mute", "vol-", "vol+", "switch"), rows[0])
        assertEquals(listOf("caps", "ctrl", "meta", "alt", "tab", "del", "fn", "left", "up", "down", "right", "hide"), rows[1])
        assertEquals(26, rows.flatten().toSet().size)
    }

    @Test fun threeRowsPreserveTheInvertedTAndAll26Keys() {
        val rows = LandscapeSystemLayout.rows(135f, fn = false, mac = false)
        assertEquals(listOf("esc", "copy", "paste", "prev", "play", "next", "mute", "vol-", "vol+", "switch"), rows[0])
        assertEquals(listOf("caps", "home", "end", "pgup", "pgdn", "fn", "up", "hide"), rows[1])
        assertEquals(listOf("ctrl", "meta", "alt", "tab", "del", "left", "down", "right"), rows[2])
        assertEquals(26, rows.flatten().toSet().size)
    }

    @Test fun fnReplacesEverythingBetweenEscAndSwitchInBothLayouts() {
        for (h in listOf(134f, 135f)) {
            val plain = LandscapeSystemLayout.rows(h, fn = false, mac = false)
            val fn = LandscapeSystemLayout.rows(h, fn = true, mac = false)
            assertEquals(listOf("esc") + (1..12).map { "f$it" } + "switch", fn[0])
            assertEquals(plain.drop(1), fn.drop(1))
        }
    }

    @Test fun macModifierOrderIsControlOptionCommandInBothLayouts() {
        for (h in listOf(134f, 135f)) {
            val keys = LandscapeSystemLayout.rows(h, fn = false, mac = true).flatten()
            assertTrue(keys.indexOf("ctrl") < keys.indexOf("alt"))
            assertTrue(keys.indexOf("alt") < keys.indexOf("meta"))
        }
    }
}
