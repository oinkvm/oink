package com.hachiwii.oink.control

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * docs/control-page.md §4.1–4.2: taps, tap-and-drag, the swapped scroll, the edge lock and its relay, and how the
 * page hands multi-finger touches to the core's shared recognizer (its own tests: `trackpad::gestures`).
 */
class TouchpadModelTest {
    private val log = ArrayList<String>()
    private var moved = 0f to 0f
    private var scrolled = 0f to 0f
    /** (t, since) of each pointer movement. */
    private val moveTimes = ArrayList<Pair<Long, Long>>()
    private val sink = object : PadSink {
        override fun move(dx: Float, dy: Float, t: Long, since: Long) {
            moved = moved.first + dx to moved.second + dy; moveTimes += t to since
            if (log.lastOrNull() != "move") log += "move"
        }
        override fun scroll(dx: Float, dy: Float, phase: PadScrollPhase, t: Long, since: Long) {
            scrolled = scrolled.first + dx to scrolled.second + dy; phases += phase
            if (log.lastOrNull() != "scroll") log += "scroll"
        }
        override fun gesture(gesture: PadGesture) { gestures += gesture }
        override fun button(button: ControlButton, down: Boolean) { log += "$button ${if (down) "down" else "up"}" }
        override fun touches(t: Long, fingers: List<PadFinger>, width: Float, height: Float, cancelled: Boolean, multiFinger: Boolean): CoreFrame {
            contacts += fingers.size to cancelled
            lastContacts = fingers.map { it.x to it.y }; contactSurface = width to height
            offered += multiFinger
            val c = claim
            return CoreFrame(c) { forwarded++; if (logCore && c != CoreClaim.UNCLAIMED) log += "core" }
        }
    }
    /** What the fake core recognizer answers; whether each report offered the fingers to it; frames forwarded. */
    private var claim = CoreClaim.UNCLAIMED
    private val offered = ArrayList<Boolean>()
    private var forwarded = 0
    /** The core's forwarded outputs go into [log] as "core" (where their order matters). */
    private var logCore = false
    /** Scroll phases and gestures sent, and (fingers, cancelled) of each touches report. */
    private val phases = ArrayList<PadScrollPhase>()
    private val gestures = ArrayList<PadGesture>()
    private val contacts = ArrayList<Pair<Int, Boolean>>()
    /** The last touches report's finger positions and pad size. */
    private var lastContacts = emptyList<Pair<Float, Float>>()
    private var contactSurface = 0f to 0f
    private fun pad() = TouchpadModel(sink).apply { width = 360f; height = 500f }
    private val click = listOf("LEFT down", "LEFT up")

    /** Tap, then press again within 0.3 s and drag towards (x, y). */
    private fun TouchpadModel.drag(x: Float, y: Float, t: Long = 0) {
        down(1, 180f, 250f, t); up(1, t + 50)
        down(2, 180f, 250f, t + 150); move(2, (180f + x) / 2, (250f + y) / 2, t + 170); move(2, x, y, t + 190)
    }

    @Test fun dragTicksCountTravelNotTimeOrLayoutChanges() {
        val p = pad()
        var ticks = 0
        p.onDragTick = { ticks++ }
        p.down(1, 180f, 250f, 0); p.move(1, 200f, 250f, 50); p.up(1, 300)
        assertEquals(0, ticks) // ordinary pointer motion
        p.down(1, 180f, 250f, 400); p.up(1, 450)
        p.down(2, 180f, 250f, 500)
        p.move(2, 192f, 250f, 510)
        assertEquals(1, ticks)
        p.move(2, 198f, 250f, 1500)
        p.move(2, 192f, 250f, 1510) // reversal still adds travel
        assertEquals(2, ticks)
        p.shift(100f, 0f)
        p.move(2, 292f, 250f, 5000) // no physical motion
        assertEquals(2, ticks)
        p.move(2, 304f, 250f, 5010)
        assertEquals(3, ticks)
        p.up(2, 5020)
        p.down(3, 180f, 250f, 6000); p.move(3, 204f, 250f, 6010)
        assertEquals(3, ticks) // a finished drag leaves no feedback in a new pointer move
    }

    @Test fun lockedDragRelayTicksAndReleaseStopsThem() {
        val p = pad()
        var ticks = 0
        p.onDragTick = { ticks++ }
        p.drag(350f, 250f)
        p.up(2, 200)
        assertNotNull(p.locked)
        val before = ticks
        p.down(3, 180f, 250f, 500); p.move(3, 204f, 250f, 510)
        assertEquals(before + 2, ticks)
        p.releaseLock()
        p.move(3, 228f, 250f, 520)
        assertEquals(before + 2, ticks)
    }

    @Test fun twoFingerTapsFeelImmediatelyEvenWhileRightClickWaits() {
        val p = pad()
        var taps = 0
        p.onClick = { taps++ }
        p.down(1, 100f, 100f, 0); p.down(2, 120f, 100f, 0)
        p.up(1, 40); p.up(2, 40)
        assertEquals(1, taps)
        p.down(1, 100f, 100f, 100); p.down(2, 120f, 100f, 100)
        p.up(1, 140); p.up(2, 140)
        assertEquals(2, taps)
        assertEquals(listOf(PadGesture.SmartMagnify), gestures)
    }

    @Test fun tapPressesAtOnceAndReleasesAfterTheDragWindow() {
        val p = pad()
        p.down(1, 100f, 100f, 0); p.up(1, 120)
        assertEquals(listOf("LEFT down"), log)
        assertEquals(RippleKind.CLICK, p.ripple?.kind)
        assertEquals(420L, p.tapUpAt)
        p.expireTap(300)
        assertEquals(listOf("LEFT down"), log)
        p.expireTap(420)
        assertEquals(click, log)
        assertNull(p.tapUpAt)
    }

    @Test fun slowTouchIsNoTap() {
        val p = pad()
        p.down(1, 100f, 100f, 0); p.up(1, 250)
        assertTrue(log.isEmpty())
    }

    @Test fun secondTapWithinTheWindowIsADoubleClick() {
        val p = pad()
        p.down(1, 100f, 100f, 0); p.up(1, 80)
        p.down(2, 100f, 100f, 200); p.up(2, 260)
        assertEquals(click + click, log)
        assertEquals(RippleKind.DOUBLE, p.ripple?.kind)
        assertNull(p.tapUpAt)
    }

    @Test fun pressAfterTapThatNeitherMovesNorTapsLetsGo() {
        val p = pad()
        p.down(1, 100f, 100f, 0); p.up(1, 80)
        p.down(2, 100f, 100f, 200); p.up(2, 900)
        assertEquals(click, log)
    }

    @Test fun tapThenPressAndMoveDragsWithTheButtonDown() {
        val p = pad()
        p.drag(250f, 300f)
        assertEquals(listOf("LEFT down", "move"), log)
        p.up(2, 400)
        assertEquals(listOf("LEFT down", "move", "LEFT up"), log)
    }

    @Test fun plainMoveNeverPressesAndShowsNoEdge() {
        val p = pad()
        p.down(1, 180f, 250f, 0)
        p.move(1, 104f + 200f, 250f, 10); p.move(1, 350f, 250f, 30)
        assertNull(p.edge)
        p.up(1, 60)
        assertEquals(listOf("move"), log)
        assertNull(p.locked)
    }

    /** User 2026-11-12: a two-finger tap waits 0.3 s for a second one (smart zoom), then right-clicks. */
    @Test fun twoFingerTapIsRightClickAfterTheDoubleTapWindowAndCompletesAWaitingTap() {
        val p = pad()
        p.down(1, 100f, 100f, 0); p.down(2, 150f, 100f, 40)
        p.up(1, 150); p.up(2, 220)
        assertTrue("held back", log.isEmpty())
        assertEquals(220L, p.rightTapAt)
        p.expireRightTap(400)
        assertTrue("window not over yet", log.isEmpty())
        p.expireRightTap(520)
        assertEquals(listOf("RIGHT down", "RIGHT up"), log)
        assertEquals(RippleKind.RIGHT, p.ripple?.kind)
        assertNull(p.rightTapAt)
        log.clear()
        p.down(1, 100f, 100f, 1000); p.up(1, 1050)
        p.down(1, 100f, 100f, 1100); p.down(2, 150f, 100f, 1120)
        assertEquals(click, log)
    }

    @Test fun movementBeyondSlopMovesThePointerAndIsNoTap() {
        val p = pad()
        p.down(1, 100f, 100f, 0)
        p.move(1, 104f, 100f, 10)          // inside 1.5 mm: the pointer already follows
        assertEquals(4f to 0f, moved)
        p.move(1, 130f, 110f, 30)
        p.up(1, 60)
        assertEquals(30f to 10f, moved)
        assertEquals(listOf("move"), log)
    }

    /** Like a Mac trackpad: a tap's tiny movement moves the pointer, and it is still a tap. */
    @Test fun tapWithinTheSlopStillClicksAndSendsItsTinyMovement() {
        val p = pad()
        p.down(1, 100f, 100f, 0)
        p.move(1, 100.5f, 100f, 20)        // noise (slop / 10): held back
        assertTrue(log.isEmpty())
        p.move(1, 103f, 100f, 40)
        assertEquals(3f to 0f, moved)
        assertEquals(listOf(40L to 0L), moveTimes)
        p.up(1, 100)
        assertEquals(listOf("move", "LEFT down"), log)
        assertEquals(RippleKind.CLICK, p.ripple?.kind)
        // Within the slop but too long: no tap, the movement still went out.
        log.clear(); moved = 0f to 0f
        val q = pad()
        q.down(1, 100f, 100f, 0); q.move(1, 104f, 100f, 100); q.up(1, 300)
        assertEquals(4f to 0f, moved)
        assertEquals(listOf("move"), log)
    }

    /** The slop still decides drag vs second click after a tap, and two-finger gestures. */
    @Test fun fingerAfterATapAndTwoFingerTouchesWaitForTheSlop() {
        val p = pad()
        p.down(1, 100f, 100f, 0); p.up(1, 80)
        p.down(2, 100f, 100f, 200); p.move(2, 105f, 100f, 220)
        assertEquals(listOf("LEFT down"), log)
        p.up(2, 260)
        assertEquals(click + click, log)
        val q = pad()
        q.down(1, 100f, 100f, 0); q.down(2, 140f, 100f, 20); q.move(1, 104f, 100f, 40)
        assertEquals(0f to 0f, moved)
    }

    /**
     * Two or more fingers are the core's (its recognizer is shared with iOS): nothing goes out from the page for
     * them; once the core claims the touch it is no tap and its remaining finger moves no pointer. Every event
     * also goes out as the fingers on the pad, and the core's outputs after the page's own.
     */
    @Test fun twoFingersAreTheCoresAndAClaimedTouchStaysQuiet() {
        val p = pad()
        p.down(1, 100f, 300f, 0); p.down(2, 160f, 300f, 5)
        p.move(mapOf(1L to (100f to 280f), 2L to (160f to 280f)), 20)
        assertEquals(0f to 0f, moved)
        assertTrue(phases.isEmpty())
        claim = CoreClaim.SCROLL
        p.move(mapOf(1L to (100f to 260f), 2L to (160f to 260f)), 30)
        // The core's scroll ends below two fingers and it keeps the touch: the remaining finger moves nothing.
        claim = CoreClaim.GESTURE
        p.up(1, 40); p.move(2, 220f, 200f, 50); p.up(2, 60)
        assertEquals(0f to 0f, moved)
        assertTrue("a claimed touch is no tap", log.isEmpty())
        assertNull(p.rightTapAt)
        assertTrue("every frame offered the fingers to the core", offered.all { it })
        assertEquals(listOf(1 to false, 2 to false), contacts.take(2))
        assertEquals(0 to false, contacts.last())
        assertEquals(contacts.size, forwarded)
    }

    /** The core may take a touch's fingers only while the page does not use them. */
    @Test fun theCoreMayTakeOnlyFingersThePageDoesNotUse() {
        val p = pad()
        p.drag(250f, 300f)
        p.down(3, 100f, 100f, 300)
        assertFalse("a drag's fingers stay the page's", offered.last())
        p.up(3, 320); p.up(2, 400)
        val s = pad().apply { oneFingerScroll = true }
        s.down(1, 100f, 300f, 1000); s.down(2, 160f, 300f, 1005)
        assertFalse("two fingers move the pointer here", offered.last())
        s.down(3, 220f, 300f, 1010)
        assertTrue("three are the core's", offered.last())
        s.up(1, 1100); s.up(2, 1100); s.up(3, 1100)
        s.down(1, 100f, 300f, 2000); s.move(1, 100f, 250f, 2020)
        s.down(2, 160f, 250f, 2030)
        assertFalse("scrolling with one finger", offered.last())
    }

    /** A held-back right click goes before what the core starts in the next touch. */
    @Test fun aClaimLetsAHeldRightClickGoFirst() {
        val p = pad()
        logCore = true
        p.down(1, 100f, 100f, 0); p.down(2, 150f, 100f, 0); p.up(1, 100); p.up(2, 100)
        p.down(3, 100f, 300f, 200); p.down(4, 150f, 300f, 200)
        claim = CoreClaim.SCROLL
        p.move(mapOf(3L to (100f to 280f), 4L to (150f to 280f)), 250)
        assertEquals(listOf("RIGHT down", "RIGHT up", "core"), log)
        assertNull(p.rightTapAt)
    }

    /** The system taking a touch the core took away: the core cancels what it runs on that frame; nothing to end here. */
    @Test fun theSystemCancelOfAClaimedTouchLeavesTheEndToTheCore() {
        val p = pad()
        p.down(1, 100f, 300f, 0); p.down(2, 150f, 300f, 0); p.down(3, 200f, 300f, 0)
        claim = CoreClaim.GESTURE
        p.move(mapOf(1L to (50f to 300f), 2L to (100f to 300f), 3L to (150f to 300f)), 50)
        p.cancel(60)
        assertEquals(0 to true, contacts.last())
        assertEquals(contacts.size, forwarded)
        assertTrue(log.isEmpty() && phases.isEmpty() && gestures.isEmpty())
    }

    /**
     * The pad moves under a resting finger (keyboard shown / hidden, rotation, the bar): the page
     * rebases the model, so the computer sees only the real finger motion and the contacts keep the
     * touch's frame and pad size.
     */
    @Test fun aLayoutShiftDuringAOneFingerMoveMakesNoJump() {
        val p = pad()
        p.down(1, 100f, 100f, 0); p.move(1, 130f, 100f, 20)
        assertEquals(30f to 0f, moved)
        // The pad moved 200 dp right: the same finger is now at x = -70 in the pad.
        p.shift(-200f, 0f)
        p.width = 160f
        p.move(1, -70f, 100f, 30)
        assertEquals("no motion for the layout change", 30f to 0f, moved)
        p.move(1, -65f, 100f, 40)
        assertEquals(35f to 0f, moved)
        assertEquals(listOf(135f to 100f), lastContacts)
        assertEquals(360f to 500f, contactSurface)
        p.up(1, 60)
        // The next touch is measured in the new layout.
        p.down(2, 50f, 50f, 1000)
        assertEquals(listOf(50f to 50f), lastContacts)
        assertEquals(160f to 500f, contactSurface)
    }

    @Test fun aLayoutShiftDuringTheSwappedScrollMakesNoJump() {
        val p = pad().apply { oneFingerScroll = true }
        p.down(1, 100f, 300f, 0); p.move(1, 100f, 280f, 20)
        assertEquals(-20f, scrolled.second, 1e-4f)
        // The keyboard rose and the pad moved 150 dp up: the finger is 150 dp lower in it.
        p.shift(0f, 150f)
        p.move(1, 100f, 430f, 30)
        assertEquals(-20f, scrolled.second, 1e-4f)
        p.move(1, 100f, 420f, 40)
        assertEquals(-30f, scrolled.second, 1e-4f)
        assertEquals(listOf(PadScrollPhase.BEGAN, PadScrollPhase.CHANGED, PadScrollPhase.CHANGED), phases)
    }

    /** Two two-finger taps within 0.3 s are smart zoom (the page's own gesture). */
    @Test fun twoFingerDoubleTapIsSmartZoom() {
        val p = pad()
        p.down(1, 100f, 100f, 3000); p.down(2, 150f, 100f, 3000); p.up(1, 3100); p.up(2, 3100)
        assertTrue(log.isEmpty())
        p.down(1, 100f, 100f, 3300); p.down(2, 150f, 100f, 3300)
        p.expireRightTap(3400) // the window ends while the second tap is down: its lift decides
        assertTrue(log.isEmpty())
        p.up(1, 3450); p.up(2, 3450)
        assertTrue(log.isEmpty())
        assertEquals(listOf<PadGesture>(PadGesture.SmartMagnify), gestures)
        assertNull(p.rightTapAt)

        // Too late for a second tap: the first right-clicks, the second waits again.
        p.down(1, 100f, 100f, 5000); p.down(2, 150f, 100f, 5000); p.up(1, 5100); p.up(2, 5100)
        p.expireRightTap(5400)
        assertEquals(listOf("RIGHT down", "RIGHT up"), log)
        p.down(1, 100f, 100f, 5500); p.down(2, 150f, 100f, 5500); p.up(1, 5600); p.up(2, 5600)
        assertEquals(5600L, p.rightTapAt)
        assertEquals(1, gestures.size)
    }

    @Test fun theNextTouchLetsAHeldRightClickGoFirst() {
        val p = pad()
        // Moving the pointer: the right click lands where the pointer was.
        p.down(1, 100f, 100f, 0); p.down(2, 150f, 100f, 0); p.up(1, 100); p.up(2, 100)
        p.down(3, 100f, 300f, 200); p.move(3, 130f, 300f, 250)
        assertEquals(listOf("RIGHT down", "RIGHT up", "move"), log)
        assertNull(p.rightTapAt)
        p.up(3, 300)
        log.clear()
        // A one-finger tap: the right click, then the left click.
        p.down(1, 100f, 100f, 1000); p.down(2, 150f, 100f, 1000); p.up(1, 1100); p.up(2, 1100)
        p.down(3, 100f, 100f, 1200); p.up(3, 1250)
        assertEquals(listOf("RIGHT down", "RIGHT up", "LEFT down"), log)
        // Leaving the page: the finished two-finger tap still right-clicks.
        val q = pad()
        log.clear()
        q.down(1, 100f, 100f, 0); q.down(2, 150f, 100f, 0); q.up(1, 100); q.up(2, 100)
        q.releaseAll(200)
        assertEquals(listOf("RIGHT down", "RIGHT up"), log)
    }

    @Test fun oneFingerScrollSwapsFingersAndClicksAtOnce() {
        val p = pad().apply { oneFingerScroll = true }
        p.down(1, 100f, 300f, 0); p.move(1, 100f, 250f, 20); p.up(1, 40)
        assertEquals(-50f, scrolled.second, .01f)
        p.down(1, 100f, 300f, 100); p.down(2, 150f, 300f, 105)
        p.move(1, 130f, 300f, 120); p.move(2, 180f, 300f, 120)
        assertEquals(30f, moved.first, .01f)
        p.up(1, 130); p.up(2, 131)
        log.clear()
        p.down(1, 100f, 300f, 1000); p.up(1, 1050)
        assertEquals(click, log)
        assertNull(p.tapUpAt)
    }

    /** The core takes the finger speed from touch times: one movement per pointer event, the first timed from the touch-down. */
    @Test fun pointerMovementsCarryTouchTimesOncePerEvent() {
        val p = pad()
        p.down(1, 100f, 100f, 5000); p.move(1, 130f, 100f, 5200); p.move(1, 140f, 100f, 5208)
        assertEquals(listOf(5200L to 5000L, 5208L to 5200L), moveTimes)
        p.up(1, 5300)
        moveTimes.clear(); moved = 0f to 0f
        // Two fingers moving the pointer in one event: one movement, timed from the second finger's landing.
        val s = pad().apply { oneFingerScroll = true }
        s.down(1, 100f, 300f, 1000); s.down(2, 160f, 300f, 1010)
        s.move(mapOf(1L to (120f to 300f), 2L to (180f to 300f)), 1050)
        assertEquals(listOf(1050L to 1010L), moveTimes)
        assertEquals(20f to 0f, moved)
    }

    /** With "单指划动 = 滚动" the one finger scrolls with phases (the core glides on after it), two fingers move the pointer. */
    @Test fun oneFingerScrollHasPhasesAndTwoFingerPointerHasNone() {
        val p = pad().apply { oneFingerScroll = true }
        p.down(1, 100f, 400f, 0)
        for (i in 1..6) p.move(1, 100f, 400f - i * 20f, 10L + i * 10)
        p.up(1, 75)
        assertEquals(listOf(PadScrollPhase.BEGAN) + List(5) { PadScrollPhase.CHANGED } + PadScrollPhase.ENDED, phases)
        assertEquals(0f to 0f, moved)
        phases.clear()
        p.down(1, 100f, 300f, 20_000); p.down(2, 160f, 300f, 20_005)
        for (i in 1..6) p.move(mapOf(1L to (100f + i * 20f to 300f), 2L to (160f + i * 20f to 300f)), 20_010L + i * 10)
        p.up(1, 20_075); p.up(2, 20_076)
        assertTrue(phases.isEmpty())
        assertEquals(120f, moved.first, .01f)
    }

    @Test fun threeFingersMoveNoPointerAndPressNothing() {
        val p = pad()
        p.down(1, 100f, 100f, 0); p.down(2, 140f, 100f, 5); p.down(3, 180f, 100f, 10)
        p.move(1, 100f, 200f, 30); p.up(1, 60); p.up(2, 61); p.up(3, 62)
        assertTrue(log.isEmpty())
        // The gestures are the core's; the page sends none itself.
        assertTrue(gestures.isEmpty() && phases.isEmpty())
        assertEquals(0f to 0f, moved)
    }

    @Test fun sixFingersMakeTheTouchInert() {
        val p = pad()
        for (i in 1..6) p.down(i.toLong(), 50f * i, 100f, 0)
        p.move(mapOf(1L to (50f to 300f)), 20)
        for (i in 1..6) p.up(i.toLong(), 40)
        assertTrue(log.isEmpty() && gestures.isEmpty() && phases.isEmpty())
    }

    @Test fun edgeProgressFillsDuringADragAndLiftingLocksIt() {
        val p = pad()
        var full = 0
        p.onEdgeFull = { full++ }
        p.drag(360f - 40f, 250f)
        val half = p.edge
        assertNotNull(half)
        assertEquals(.5f, half!!.progress, .01f)
        assertTrue(half.vertical)
        p.move(2, 360f - 12f, 250f, 210)
        assertTrue(p.edge!!.full)
        p.move(2, 360f - 10f, 250f, 220)
        assertEquals(1, full)
        p.up(2, 240)
        assertEquals(360f - 10f to 250f, p.locked)
        assertNull(p.edge)
        assertFalse(log.contains("LEFT up"))
    }

    @Test fun liftingBeforeFullEndsTheDrag() {
        val p = pad()
        p.drag(320f, 250f); p.up(2, 240)
        assertNull(p.locked)
        assertEquals("LEFT up", log.last())
    }

    @Test fun relayTakesOverTheLockedDragAndTapsAreNotSent() {
        val p = pad()
        p.drag(350f, 250f); p.up(2, 240)
        assertNotNull(p.locked)
        log.clear()
        // A tap while locked does nothing; the button stays down.
        p.down(3, 100f, 250f, 1000); p.up(3, 1050)
        assertTrue(log.isEmpty())
        assertNotNull(p.locked)
        // Resting is not moving (< 6 dp); the first movement takes over and keeps dragging.
        p.down(4, 100f, 250f, 2000); p.move(4, 104f, 250f, 2010)
        assertNotNull(p.locked)
        p.move(4, 112f, 250f, 2020)
        assertNull(p.locked)
        assertEquals(listOf("move"), log)
        // Drag lock (user 2026-11-12): lifting away from the edge keeps the drag locked there.
        p.up(4, 2100)
        assertEquals(listOf("move"), log)
        assertEquals(112f to 250f, p.locked)
    }

    /** macOS-style drag lock (user 2026-11-12): once locked, any number of lifts and new fingers carry one drag; only 释放 ends it. */
    @Test fun dragLockKeepsTheButtonDownThroughEveryLiftUntilRelease() {
        val p = pad()
        p.drag(350f, 250f); p.up(2, 240)
        log.clear()
        for (i in 0 until 3) {
            val t = 1000L + i * 1000
            p.down(10L + i, 100f, 250f, t); p.move(10L + i, 140f, 260f, t + 20); p.move(10L + i, 180f, 270f, t + 40)
            p.up(10L + i, t + 60)
            assertNotNull(p.locked)
            // The next finger landing changes nothing by itself.
            p.down(20L + i, 200f, 200f, t + 500)
            assertFalse(log.contains("LEFT up"))
            p.up(20L + i, t + 520) // a tap: not sent
        }
        assertEquals(listOf("move"), log)
        // A two-finger scroll while locked does not end it either.
        p.down(30, 100f, 300f, 5000); p.down(31, 160f, 300f, 5005)
        p.move(30, 100f, 280f, 5020); p.move(31, 160f, 280f, 5020)
        p.up(30, 5040); p.up(31, 5041)
        assertFalse(log.contains("LEFT up"))
        assertNotNull(p.locked)
        p.releaseLock()
        assertNull(p.locked)
        assertEquals("LEFT up", log.last())
        // The lock is over: a new drag ends when its finger lifts, as before.
        log.clear()
        p.drag(320f, 250f, 10_000); p.up(2, 10_240)
        assertNull(p.locked)
        assertEquals("LEFT up", log.last())
    }

    @Test fun eachClickTicks() {
        val p = pad()
        var ticks = 0
        p.onClick = { ticks++ }
        p.down(1, 100f, 100f, 0); p.up(1, 80)                       // click
        assertEquals(1, ticks)
        p.down(2, 100f, 100f, 200); p.up(2, 260)                     // second tap: double click
        assertEquals(2, ticks)
        p.down(1, 100f, 100f, 2000); p.down(2, 150f, 100f, 2040)    // two-finger tap: right click, 0.3 s later
        p.up(1, 2150); p.up(2, 2220)
        assertEquals(3, ticks) // feedback follows the tap immediately, not the deferred right click
        p.expireRightTap(2520)
        assertEquals(3, ticks)
        p.down(1, 100f, 100f, 4000); p.move(1, 150f, 100f, 4020); p.up(1, 4040) // a move is no click
        assertEquals(3, ticks)
    }

    @Test fun releaseCapsuleEndsTheLockedDrag() {
        val p = pad()
        p.drag(350f, 250f); p.up(2, 240)
        p.releaseLock()
        assertNull(p.locked)
        assertEquals("LEFT up", log.last())
    }

    @Test fun leavingThePageLetsTheButtonGo() {
        val p = pad()
        p.drag(350f, 250f); p.up(2, 240)
        p.releaseAll(9_000)
        assertNull(p.locked)
        assertEquals("LEFT up", log.last())
        log.clear()
        p.down(1, 100f, 100f, 5000); p.up(1, 5050)
        p.releaseAll(9_000)
        assertEquals(click, log)
    }

    @Test fun cornerGlowsAtTheCorner() {
        val e = EdgeLock.of(340f, 480f, 360f, 500f)!!
        assertTrue(e.corner)
        assertEquals(360f, e.cx); assertEquals(500f, e.cy)
        assertNull(EdgeLock.of(180f, 250f, 360f, 500f))
    }
}
