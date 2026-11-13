package com.hachiwii.oink.control

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The landscape floating keyboard's handle (user 2026-11-12, docs/control-page.md §1.A). */
class FloatMotionTest {
    private val eps = 1e-3f

    @Test fun referencePortraitWidthStaysInsideSafeAreas() {
        assertEquals(392.72727f, FloatMotion.width(873f, 36f, 0f), eps)
        assertEquals(348f, FloatMotion.width(400f, 36f, 0f), eps)
        assertEquals(0f, FloatMotion.width(40f, 36f, 0f), eps)
    }

    @Test fun appleProjectionIsHalfASecondOfVelocity() {
        // r = 0.998: position + v ÷ 1000 × 499.
        assertEquals(100f + 499f, FloatMotion.project(100f, 1000f), .01f)
        assertEquals(100f, FloatMotion.project(100f, 0f), eps)
    }

    @Test fun boundsStopAtTheCutoutSideAndEightFromTheFreeEdge() {
        // 893 wide, cutout 36 on the left, gesture navigation (nothing on the sides).
        val left = FloatMotion.Bounds.of(893f, 400f, cutoutLeft = 36f, cutoutRight = 0f, freeLeft = 0f, freeRight = 0f, cutoutOnRight = false)
        assertEquals(36f, left.lo, eps)
        assertEquals(893f - 8f - 400f, left.hi, eps)
        assertEquals("default: away from the cutout", left.hi, left.home(cutoutRight = false), eps)
        val right = FloatMotion.Bounds.of(893f, 400f, cutoutLeft = 0f, cutoutRight = 36f, freeLeft = 0f, freeRight = 0f, cutoutOnRight = true)
        assertEquals(8f, right.lo, eps)
        assertEquals(893f - 36f - 400f, right.hi, eps)
        assertEquals(right.lo, right.home(cutoutRight = true), eps)
    }

    @Test fun pastABoundTheContainerMovesAThird() {
        val b = FloatMotion.Bounds(36f, 485f)
        assertEquals(36f - 10f, b.rubber(6f), eps)
        assertEquals(485f + 10f, b.rubber(515f), eps)
        assertEquals(200f, b.rubber(200f), eps)
    }

    @Test fun aFlickRestsAtTheProjectionClampedAndSnappedToABound() {
        val b = FloatMotion.Bounds(36f, 485f)
        // No velocity: stays.
        assertEquals(250f, b.rest(250f, 0f), eps)
        // A flick to the left far past the bound: the bound.
        assertEquals(36f, b.rest(300f, -2000f), eps)
        // Projected within 24 of a bound: snapped.
        assertEquals(485f, b.rest(400f, (470f - 400f) / .499f), .5f)
        // Projected elsewhere: there.
        assertEquals(300f, b.rest(250f, 50f / .499f), .5f)
    }

    @Test fun dismissShrinksAndFadesAndAFlickFinishesIt() {
        val (s80, o80) = FloatMotion.dismissLook(80f, 0f)
        assertEquals(.875f, s80, eps); assertEquals(.7f, o80, eps)
        val (s160, o160) = FloatMotion.dismissLook(160f, 0f)
        assertEquals(.75f, s160, eps); assertEquals(.4f, o160, eps)
        val (sEnd, oEnd) = FloatMotion.dismissLook(80f, 1f)
        assertEquals(.6f, sEnd, eps); assertEquals(0f, oEnd, eps)
        // A slow drag has to travel the 80; a short quick flick does it.
        assertFalse(FloatMotion.dismisses(79f, 0f))
        assertTrue(FloatMotion.dismisses(81f, 0f))
        assertTrue(FloatMotion.dismisses(20f, 300f))
    }

    @Test fun doubleTapTransitionShowsKeysThenTheIcon() {
        val start = FloatMotion.morphLook(0f)
        assertEquals(1f, start.floating, eps); assertEquals(0f, start.icon, eps)
        val mid = FloatMotion.morphLook(.7f)
        assertEquals(0f, mid.floating, eps); assertEquals(1f, mid.icon, eps)
    }

    @Test fun springIsCriticallyDampedWithAResponseOf035Seconds() {
        assertEquals(322.3f, FloatMotion.STIFFNESS, .5f)
    }
}
