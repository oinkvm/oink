package com.hachiwii.oink.control

import com.hachiwii.oink.control.Fullscreen.Request
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** 全屏 / 恢复 (user 2026-11-12, docs/control-page.md §1.A): which orientation the control page asks for. */
class FullscreenTest {
    @Test fun rotationFollowsTheSystemUntilRestoredThenNotUntilThePageIsLeft() {
        val f = Fullscreen()
        assertEquals(Request.SYSTEM, f.request)
        // Entered by turning the phone: nothing to ask. 恢复 turns the interface back and keeps it upright…
        f.restore(upright = false)
        assertEquals(Request.PORTRAIT, f.request)
        // … whatever the phone does, until the user leaves the page.
        f.tilt(upright = true)
        assertEquals(Request.PORTRAIT, f.request)
        f.left()
        assertEquals(Request.SYSTEM, f.request)
    }

    @Test fun theButtonTurnsToLandscapeAndRestoreGivesRotationBackOnceUpright() {
        val f = Fullscreen()
        f.enter()
        assertEquals(Request.LANDSCAPE, f.request)
        // 恢复 while the phone is still sideways: portrait, until the phone is upright.
        f.restore(upright = false)
        assertEquals(Request.PORTRAIT, f.request)
        assertTrue(f.watchesTilt)
        f.tilt(upright = false)
        assertEquals(Request.PORTRAIT, f.request)
        f.tilt(upright = true)
        assertEquals(Request.SYSTEM, f.request)
        assertFalse(f.watchesTilt)
    }

    @Test fun restoreFromTheButtonWhileUprightGivesRotationBackAtOnce() {
        val f = Fullscreen()
        f.enter()
        f.restore(upright = true)
        assertEquals(Request.SYSTEM, f.request)
        // Unknown tilt (flat on a table) keeps portrait until the phone is seen upright.
        f.enter()
        f.restore(upright = null)
        assertEquals(Request.PORTRAIT, f.request)
    }

    @Test fun theButtonAfterARestoredRotationStillReturnsToTheHeldPortrait() {
        val f = Fullscreen()
        f.restore(upright = false)
        f.enter()
        assertEquals(Request.LANDSCAPE, f.request)
        f.restore(upright = true)
        assertEquals("no auto full screen until the page is left", Request.PORTRAIT, f.request)
        f.left()
        assertEquals(Request.SYSTEM, f.request)
    }

    @Test fun leavingThePageEndsAButtonFullScreen() {
        val f = Fullscreen()
        f.enter()
        f.left()
        assertEquals(Request.SYSTEM, f.request)
    }

    @Test fun tiltFromOrientationListenerDegrees() {
        assertEquals(true, Fullscreen.upright(0))
        assertEquals(true, Fullscreen.upright(350))
        assertEquals(true, Fullscreen.upright(180))
        assertEquals(false, Fullscreen.upright(90))
        assertEquals(false, Fullscreen.upright(270))
        assertNull(Fullscreen.upright(45))
        assertNull(Fullscreen.upright(-1))
    }
}
