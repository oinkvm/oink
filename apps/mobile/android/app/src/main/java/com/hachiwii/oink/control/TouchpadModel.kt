package com.hachiwii.oink.control

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlin.math.hypot
import kotlin.math.max
import kotlin.math.min

/** The phase of a touchpad scroll of the fingers; the core adds the Mac's momentum after [ENDED]. */
enum class PadScrollPhase { BEGAN, CHANGED, ENDED, CANCELLED }

/** A gesture the page decides itself (the core's `ControlGesture`); the multi-finger gestures are the core's. */
sealed interface PadGesture {
    /** Two-finger double tap. */
    data object SmartMagnify : PadGesture
}

/** What the core's shared multi-finger recognizer does with the current touch (the core's `ControlTouchClaim`). */
enum class CoreClaim {
    /** Nothing (yet): the touch is the page's. */
    UNCLAIMED,
    /** A two-finger scroll. */
    SCROLL,
    /** A gesture, or a touch whose gesture is over: nothing more happens here (no tap, its remaining fingers move no pointer). */
    GESTURE,
}

/** The core's recognizer on one touch event: its [claim], and [forward], which sends its outputs (after the page's own for the event). */
class CoreFrame(val claim: CoreClaim, val forward: () -> Unit = {}) {
    companion object {
        val UNCLAIMED = CoreFrame(CoreClaim.UNCLAIMED)
    }
}

/** One finger on the pad, dp from its top left. */
data class PadFinger(val id: Long, val x: Float, val y: Float)

/** Where the touchpad's commands go (the control link; a button going up also lets held modifiers go). */
interface PadSink {
    /**
     * One pointer movement per pointer event: [t] is the event's `uptimeMillis` and [since] the time the
     * movement is measured from (the previous movement, or the touch-down / finger change for the first).
     */
    fun move(dx: Float, dy: Float, t: Long, since: Long)
    /** The swapped one-finger scroll (dp, finger direction) with its phase, timed like [move]; two-finger scrolls are the core's. */
    fun scroll(dx: Float, dy: Float, phase: PadScrollPhase, t: Long, since: Long)
    /** A gesture the page decided itself (the two-finger double tap's smart zoom). */
    fun gesture(gesture: PadGesture)
    fun button(button: ControlButton, down: Boolean)
    /**
     * Every finger on the pad after a touch event (also none), the pad's size in dp: the trackpad's raw contacts,
     * and the same frame through the core's multi-finger recognizer (shared with iOS); [multiFinger]: the touch's
     * fingers are free for it. Returns the recognizer's claim; the model calls [CoreFrame.forward] after its own
     * commands for the event.
     */
    fun touches(t: Long, fingers: List<PadFinger>, width: Float, height: Float, cancelled: Boolean, multiFinger: Boolean): CoreFrame
}

enum class RippleKind { CLICK, DOUBLE, RIGHT }
data class Ripple(val kind: RippleKind, val x: Float, val y: Float, val id: Long)

/**
 * Edge lock (docs/control-page.md §4.2) while a one-finger drag nears the pad's edge: the glow is
 * centred on ([cx], [cy]) (nearest edge point, or the corner), the lock badge sits at ([lx], [ly]),
 * and [progress] runs 0 → 1 from [EDGE_START] to [EDGE_FULL] dp from the edge.
 */
data class EdgeLock(val corner: Boolean, val vertical: Boolean, val cx: Float, val cy: Float, val lx: Float, val ly: Float, val progress: Float) {
    val full get() = progress >= 1f
    companion object {
        const val EDGE_START = 64f
        const val EDGE_FULL = 16f
        const val LOCK_GAP = 46f
        fun of(x: Float, y: Float, w: Float, h: Float): EdgeLock? {
            val dx = min(x, w - x); val dy = min(y, h - y); val d = min(dx, dy)
            if (d >= EDGE_START) return null
            val p = ((EDGE_START - d) / (EDGE_START - EDGE_FULL)).coerceIn(0f, 1f)
            val sx = if (x < w / 2) 0f else w; val sy = if (y < h / 2) 0f else h
            val ix = if (sx == 0f) 1f else -1f; val iy = if (sy == 0f) 1f else -1f
            val inset = d + LOCK_GAP
            return when {
                dx < EDGE_START && dy < EDGE_START -> EdgeLock(true, false, sx, sy, sx + ix * inset * .71f, sy + iy * inset * .71f, p)
                dx < dy -> EdgeLock(false, true, sx, y, sx + ix * inset, y, p)
                else -> EdgeLock(false, false, x, sy, x, sy + iy * inset, p)
            }
        }
    }
}

/**
 * Touchpad gestures in dp and milliseconds (docs/control-page.md §4.1–4.2, same rules as the iOS page
 * and protocol-v2.md §14.6): one finger moves the pointer from its first movement (only ~0.15 mm of
 * noise is held back; a touch within the 1.5 mm slop for under [TAP_MS] is still a tap, and the little
 * it moved is sent, as on a Mac trackpad); a tap presses the left button at once and
 * lets it go [DRAG_ARM_MS] later unless the next finger lands first — moving on then drags with the
 * button still down (tap-and-drag, always on), tapping again makes a double click. Two fingers tap
 * for a right click (a second two-finger tap soon after is smart zoom); "单指划动 = 滚动"
 * swaps one and two fingers (one finger scrolls, two move the pointer; no dragging, no edge lock then).
 * Only a drag shows the edge lock: lifting with the ring full locks the drag (drag lock, like macOS's, user
 * 2026-11-12): the button stays down through every later lift, each new finger that moves carries
 * on the same drag, a tap is not sent, and only 释放 (or leaving the page) lets the button go.
 *
 * The phone is a Mac trackpad source (user 2026-11-12). Two or more moving fingers are the core's (user
 * 2026-11-12: Android and iOS identical, as a Mac trackpad): every touch frame goes to its shared recognizer
 * ([PadSink.touches]), which decides two-finger scroll, pinch and rotate and the Dock gestures (three / four
 * finger swipes, four / five finger pinch) as the Mac's trackpad driver does, and answers with its claim on the
 * touch; the frame is also the raw contacts for Windows' own recognition.
 */
class TouchpadModel(private val sink: PadSink, private val slop: Float = SLOP_DP) {
    /** Finger noise held back before a lone finger moves the pointer (≈ 0.15 mm). */
    private val noise = slop / 10
    var oneFingerScroll = false
    var width = 0f; var height = 0f

    /** First finger while any is down (the press highlight follows it). */
    var pressed by mutableStateOf<Pair<Float, Float>?>(null); private set
    var edge by mutableStateOf<EdgeLock?>(null); private set
    /** A dragging finger kept on the pad after lifting at the edge; the release capsule shows while set. */
    var locked by mutableStateOf<Pair<Float, Float>?>(null); private set
    var ripple by mutableStateOf<Ripple?>(null); private set
    /** When a tap's deferred button-up is due (the page calls [expireTap] then); null when none is waiting. */
    var tapUpAt by mutableStateOf<Long?>(null); private set
    /**
     * When a held-back two-finger tap ended (the page calls [expireRightTap] [TWO_FINGER_DOUBLE_TAP_MS]
     * later); null when none is waiting.
     */
    var rightTapAt by mutableStateOf<Long?>(null); private set
    /** Called once each time the edge-lock ring fills (haptic). */
    var onEdgeFull: () -> Unit = {}
    /** Called for each completed tap (including two-finger taps before right-click deferral). */
    var onClick: () -> Unit = {}
    /** One native selection tick per 12 dp of drag travel, including drag-lock relay. */
    var onDragTick: () -> Unit = {}
    private var dragTravel = 0f
    /**
     * Diagnostics (the page logs to logcat, rate-limited): finger count changes, the multi-finger
     * decision with its measures, gesture begin / end. Never per move.
     */
    var log: (String) -> Unit = {}

    /**
     * PENDING: one finger, undecided. PAN: one finger moving (pointer, drag, or the swapped scroll).
     * TWO: two or more fingers, undecided (the core's to decide, except the swapped two-finger pointer).
     * TWO_POINTER: two fingers moving the pointer ("单指划动 = 滚动"). CORE: the core took the touch.
     */
    private enum class Mode { IDLE, PENDING, PAN, TWO, TWO_POINTER, CORE, ENDED, INERT }
    private var mode = Mode.IDLE
    private val points = LinkedHashMap<Long, FloatArray>()
    /** This touch dragged with the left button down: its fingers stay the page's (the core never takes them). */
    private var dragTouch = false
    private var firstId = -1L
    private var t0 = 0L
    private var maxFingers = 0
    private var sx = 0f; private var sy = 0f          // where the gesture started (first finger / centroid)
    private var lx = 0f; private var ly = 0f          // last position sent from
    private var lt = 0L                               // its touch time
    private var hadLock = false
    /** The left button is down on the remote: a tap waiting for its up, a drag, or a locked drag. */
    private var leftDown = false
    /** This touch landed while a tap's up was waiting: moving drags, tapping double-clicks. */
    private var afterTap = false
    private var dragging = false
    /** The drag is locked (the ring filled once): lifts no longer end it, only 释放 does. */
    private var dragLock = false
    private var lastClickAt = Long.MIN_VALUE / 2
    private var rightX = 0f; private var rightY = 0f
    private var rippleSeq = 0L
    /** The swapped one-finger scroll's Began went out. */
    private var scrolling = false
    /**
     * The contacts keep the frame and pad size of the touch's start: a layout change mid-touch (which
     * moves or resizes the pad) must not move them on the target. [frameX], [frameY]: how far the
     * pad's coordinates have shifted since the first finger landed.
     */
    private var frameX = 0f; private var frameY = 0f
    private var touchWidth = 0f; private var touchHeight = 0f

    private fun centroid(of: Map<Long, FloatArray> = points): Pair<Float, Float> {
        var x = 0f; var y = 0f
        of.values.forEach { x += it[0]; y += it[1] }
        return x / of.size to y / of.size
    }

    private fun leftUp() {
        if (!leftDown) return
        dragTravel = 0f
        leftDown = false; dragging = false; dragLock = false; afterTap = false; tapUpAt = null
        sink.button(ControlButton.LEFT, false)
    }

    /** The fingers to the core (contacts and its recognizer); asked before this model handles the event. */
    private fun report(t: Long, cancelled: Boolean = false): CoreFrame =
        sink.touches(t, points.map { (id, p) -> PadFinger(id, p[0] - frameX, p[1] - frameY) }, touchWidth, touchHeight, cancelled,
            coreMayRecognize())

    /**
     * Whether the core may take this touch's fingers (the fingers now on the pad): not while this model uses
     * them — a drag, the swapped one-finger scroll, two fingers moving the pointer ("单指划动 = 滚动"), an
     * inert or finished touch.
     */
    private fun coreMayRecognize(): Boolean = when (mode) {
        Mode.CORE -> true
        Mode.IDLE, Mode.PENDING, Mode.TWO -> !dragTouch && !(oneFingerScroll && points.size == 2)
        Mode.PAN -> !dragging && !dragTouch && !oneFingerScroll
        Mode.TWO_POINTER, Mode.ENDED, Mode.INERT -> false
    }

    /** The core took the touch: from now on it is the core's; a held-back right click goes first (its outputs follow). */
    private fun adopt(claim: CoreClaim) {
        if (claim == CoreClaim.UNCLAIMED || mode == Mode.CORE || !coreMayRecognize()) return
        fireRight()
        if (afterTap) leftUp()
        afterTap = false
        edge = null
        log("core took the touch: $claim fingers=${points.size} (was $mode)")
        mode = Mode.CORE
    }

    /**
     * The pad moved under the fingers by ([dx], [dy]) dp (a layout change during a touch: the keyboard,
     * a rotation, the tab bar): the fingers did not move, so every remembered position moves into the
     * pad's new coordinates and the next event measures only real finger motion.
     */
    fun shift(dx: Float, dy: Float) {
        if (points.isEmpty() || (dx == 0f && dy == 0f)) return
        for (p in points.values) { p[0] += dx; p[1] += dy }
        sx += dx; sy += dy; lx += dx; ly += dy
        frameX += dx; frameY += dy
        log("layout shift during a touch: %.1f, %.1f dp".format(dx, dy))
    }

    /** The movement's start is here: centroid and the time the next delta is measured from. */
    private fun restart(t: Long) {
        val (cx, cy) = centroid(); sx = cx; sy = cy; lx = cx; ly = cy; lt = t
    }

    /** Ends the swapped one-finger scroll: cancelled (the system) or ended (lift). The core ends what it runs. */
    private fun endRunning(t: Long, cancel: Boolean) {
        if (mode == Mode.PAN && oneFingerScroll) endScroll(t, cancel)
    }

    /** Sends a gesture this model decided (logged). */
    private fun gesture(g: PadGesture) {
        log("gesture sent: $g")
        sink.gesture(g)
    }

    private fun endScroll(t: Long, cancel: Boolean) {
        if (!scrolling) return
        scrolling = false
        sink.scroll(0f, 0f, if (cancel) PadScrollPhase.CANCELLED else PadScrollPhase.ENDED, t, lt)
    }

    fun down(id: Long, x: Float, y: Float, t: Long) {
        if (points.isEmpty()) { frameX = 0f; frameY = 0f; touchWidth = width; touchHeight = height; dragTouch = false }
        points[id] = floatArrayOf(x, y)
        val core = report(t)
        maxFingers = max(maxFingers, points.size)
        val before = mode
        try { downMode(id, x, y, t); adopt(core.claim) } finally {
            log("finger down: fingers=${points.size} mode $before -> $mode scrolling=$scrolling t=${t - t0}ms")
        }
        core.forward()
    }

    private fun downMode(id: Long, x: Float, y: Float, t: Long) {
        when {
            points.size == 1 -> {
                mode = Mode.PENDING; firstId = id; t0 = t; sx = x; sy = y; lx = x; ly = y; lt = t; hadLock = locked != null
                maxFingers = 1; scrolling = false
                restart(t)
                pressed = x to y
                // The next finger within DRAG_ARM_MS of a tap: the button stays down while we see what it does.
                afterTap = tapUpAt != null
                tapUpAt = null
            }
            // The core's: more fingers are its business (it cancels a running gesture).
            mode == Mode.CORE -> {}
            points.size > MAX_FINGERS -> { endRunning(t, cancel = true); mode = Mode.INERT; edge = null }
            // The swapped one-finger scroll keeps its finger until it lifts.
            scrolling -> {}
            mode == Mode.PENDING || mode == Mode.PAN || mode == Mode.TWO || mode == Mode.TWO_POINTER -> {
                // Another finger: glow and progress go at once; a waiting tap completes first; the touch
                // is decided again from here (the core may take a pointer move's fingers).
                if (afterTap) leftUp()
                mode = Mode.TWO
                edge = null
                restart(t)
            }
            else -> {}
        }
    }

    fun move(id: Long, x: Float, y: Float, t: Long) = move(mapOf(id to (x to y)), t)

    /**
     * One pointer event: every finger that moved in it (dp) at the event's time [t]. The fingers move
     * once per event, so two fingers moving the pointer make one movement, not two a few microseconds
     * apart (which read as a huge finger speed).
     */
    fun move(moved: Map<Long, Pair<Float, Float>>, t: Long) {
        var known = false
        for ((id, at) in moved) {
            val p = points[id] ?: continue
            p[0] = at.first; p[1] = at.second
            known = true
        }
        if (!known) return
        val core = report(t)
        adopt(core.claim)
        moveMode(moved, t)
        core.forward()
    }

    private fun moveMode(moved: Map<Long, Pair<Float, Float>>, t: Long) {
        val first = points[firstId]
        if (first != null && firstId in moved) pressed = first[0] to first[1]
        when (mode) {
            Mode.PENDING -> {
                // One finger only (a second one makes it TWO).
                val p = first ?: return
                val x = p[0]; val y = p[1]
                // While a drag is locked the new finger takes over at its first 6 dp of movement.
                val threshold = if (hadLock) RELAY_DP else slop
                if (hypot(x - sx, y - sy) > threshold) {
                    // Not a second two-finger tap: a held-back right click goes first.
                    fireRight()
                    mode = Mode.PAN
                    if (locked != null) locked = null
                    if (afterTap && !oneFingerScroll) dragging = true else if (afterTap) leftUp()
                    afterTap = false
                    // A drag (or a locked drag's relay) keeps its fingers from the core.
                    if (dragging) dragTouch = true
                    pan(x, y, t)
                } else if (!hadLock && !afterTap && !oneFingerScroll && hypot(x - lx, y - ly) > noise) {
                    // A lone finger that may still be a tap moves the pointer from its first movement,
                    // holding back only the noise (like a Mac trackpad); it stays a tap while within the
                    // slop. After a tap (drag or second click) or while a drag is locked (relay) the slop
                    // decides first. A held-back right click goes first, where the pointer still is.
                    fireRight()
                    pan(x, y, t)
                }
            }
            Mode.PAN -> if (first != null && firstId in moved) pan(first[0], first[1], t)
            Mode.TWO -> decide(t)
            Mode.TWO_POINTER -> { val (cx, cy) = centroid(); twoFinger(cx, cy, t) }
            else -> {}
        }
    }

    /**
     * Two or more fingers, undecided: they are the core's (it takes the touch through its claim). Decided here,
     * once they pass the slop: the swapped two-finger pointer ("单指划动 = 滚动") and the finger left of a
     * multi-finger touch the core did not take (it moves the pointer, or scrolls swapped, as on iOS).
     */
    private fun decide(t: Long) {
        val n = points.size
        if (n >= 2 && !(oneFingerScroll && n == 2)) return
        val (cx, cy) = centroid()
        val travel = hypot(cx - sx, cy - sy)
        if (travel <= slop) return
        val decided = mode
        // Not a second two-finger tap: a held-back right click goes first.
        fireRight()
        mode = if (n == 1) Mode.PAN else Mode.TWO_POINTER
        if (mode == Mode.PAN) pan(cx, cy, t) else twoFinger(cx, cy, t)
        log("decide: fingers=$n travel=%.1f slop=%.1f -> $mode (was $decided)".format(travel, slop))
    }

    private fun pan(x: Float, y: Float, t: Long) {
        val dx = x - lx; val dy = y - ly
        val since = lt
        lx = x; ly = y; lt = t
        if (oneFingerScroll) { scroll(dx, dy, t, since); return }
        sink.move(dx, dy, t, since)
        if (!dragging) return
        dragTravel += hypot(dx, dy)
        while (dragTravel >= DRAG_TICK_DP) {
            dragTravel -= DRAG_TICK_DP
            onDragTick()
        }
        val next = EdgeLock.of(x, y, width, height)
        if (next?.full == true && edge?.full != true) onEdgeFull()
        edge = next
    }

    /** The swapped two-finger pointer ("单指划动 = 滚动"). */
    private fun twoFinger(cx: Float, cy: Float, t: Long) {
        val dx = cx - lx; val dy = cy - ly
        val since = lt
        lx = cx; ly = cy; lt = t
        sink.move(dx, dy, t, since)
    }

    private fun scroll(dx: Float, dy: Float, t: Long, since: Long) {
        sink.scroll(dx, dy, if (scrolling) PadScrollPhase.CHANGED else PadScrollPhase.BEGAN, t, since)
        scrolling = true
    }

    fun up(id: Long, t: Long) {
        val before = mode
        val p = points.remove(id) ?: return
        val core = report(t)
        try { adopt(core.claim); upMode(id, p, t) } finally {
            log("finger up: fingers=${points.size} mode $before -> $mode t=${t - t0}ms")
        }
        core.forward()
    }

    private fun upMode(id: Long, p: FloatArray, t: Long) {
        if (id == firstId) pressed = null
        if (points.isNotEmpty()) {
            when (mode) {
                // The swapped two-finger pointer is over below two fingers (the rest of this touch does nothing);
                // with fingers left it goes on from their centroid (no jump).
                Mode.TWO_POINTER -> if (points.size < 2) mode = Mode.ENDED
                    else { val (cx, cy) = centroid(); lx = cx; ly = cy; lt = t }
                // The swapped one-finger scroll goes on while its finger stays; when it lifts, it ends.
                Mode.PAN -> if (id == firstId || !scrolling) { endRunning(t, cancel = false); edge = null; mode = Mode.ENDED }
                Mode.TWO -> restart(t)
                else -> {}
            }
            if (pressed == null) points.entries.firstOrNull()?.let { firstId = it.key; pressed = it.value[0] to it.value[1] }
            return
        }
        val m = mode
        endRunning(t, cancel = false)
        // A second two-finger tap that started within the window of a held-back one: smart zoom, and
        // no right click at all. Any other touch lets the held-back right click go first.
        val twoFingerTap = m == Mode.TWO && !hadLock && maxFingers == 2 && t - t0 < TWO_FINGER_TAP_MS
        val smart = twoFingerTap && rightTapAt?.let { t0 - it <= TWO_FINGER_DOUBLE_TAP_MS } == true
        if (!smart) fireRight()
        mode = Mode.IDLE
        pressed = null
        // The ring full at the lift starts the drag lock; once locked, every lift keeps the drag
        // (the lifted finger stays as the locked one, user 2026-11-12).
        val lock = dragging && (dragLock || (edge?.full == true && m == Mode.PAN))
        edge = null
        when (m) {
            Mode.PENDING -> when {
                // A tap while a drag is locked is not sent, so the drag is not interrupted.
                hadLock -> {}
                t - t0 >= TAP_MS -> leftUp() // a press after a tap that neither moved nor tapped
                afterTap -> {
                    // Second tap: the first click completes, then a whole second click.
                    leftUp()
                    sink.button(ControlButton.LEFT, true); sink.button(ControlButton.LEFT, false)
                    lastClickAt = Long.MIN_VALUE / 2
                    showRipple(RippleKind.DOUBLE, sx, sy)
                }
                else -> {
                    // Down now, up after DRAG_ARM_MS unless the next finger lands first (the ripple shows now).
                    val double = t - lastClickAt < DOUBLE_CLICK_MS
                    lastClickAt = if (double) Long.MIN_VALUE / 2 else t
                    sink.button(ControlButton.LEFT, true)
                    // With one finger scrolling there is no dragging, so nothing to wait for.
                    if (oneFingerScroll) sink.button(ControlButton.LEFT, false)
                    else { leftDown = true; tapUpAt = t + DRAG_ARM_MS }
                    showRipple(if (double) RippleKind.DOUBLE else RippleKind.CLICK, sx, sy)
                }
            }
            Mode.TWO -> if (twoFingerTap) {
                onClick()
                lastClickAt = Long.MIN_VALUE / 2
                if (smart) {
                    rightTapAt = null
                    gesture(PadGesture.SmartMagnify)
                } else {
                    // Held back for a possible second two-finger tap (smart zoom).
                    rightTapAt = t; rightX = sx; rightY = sy
                }
            }
            else -> {}
        }
        if (lock) { if (locked == null) locked = p[0] to p[1]; dragLock = true }
        else if (locked == null && tapUpAt == null) leftUp()
    }

    /** The held-back two-finger tap right-clicks now (its window ended, or the next touch is something else). */
    private fun fireRight() {
        if (rightTapAt == null) return
        rightTapAt = null
        sink.button(ControlButton.RIGHT, true); sink.button(ControlButton.RIGHT, false)
        showRipple(RippleKind.RIGHT, rightX, rightY, feel = false)
    }

    /**
     * The window after a two-finger tap ended: it right-clicks, unless a touch that may be the second
     * two-finger tap is still on the pad (its lift decides).
     */
    fun expireRightTap(t: Long) {
        val at = rightTapAt ?: return
        if (t - at < TWO_FINGER_DOUBLE_TAP_MS) return
        if (points.isNotEmpty() && (mode == Mode.PENDING || mode == Mode.TWO) && maxFingers <= 2 && t0 - at <= TWO_FINGER_DOUBLE_TAP_MS) return
        fireRight()
    }

    /** The waiting tap's time is up: its button goes up (a plain click, 0.3 s late as on a Mac with dragging on). */
    fun expireTap(t: Long) {
        val at = tapUpAt ?: return
        if (t >= at && points.isEmpty()) leftUp()
    }

    /** The system took the gesture away: forget fingers, no click, a scroll or gesture is cancelled; a drag lets go unless it is locked. */
    fun cancel(t: Long) {
        log("cancel: fingers=${points.size} mode=$mode")
        // The core cancels what it runs on this frame.
        val core = if (points.isNotEmpty()) { points.clear(); report(t, cancelled = true) } else CoreFrame.UNCLAIMED
        endRunning(t, cancel = true)
        mode = Mode.IDLE; pressed = null; edge = null
        // A held-back right click whose window ran out while this touch was deciding goes now.
        rightTapAt?.let { if (t - it >= TWO_FINGER_DOUBLE_TAP_MS) fireRight() }
        if (dragLock && locked == null) locked = lx to ly
        if (locked == null) leftUp()
        core.forward()
    }

    /** 释放: the locked finger lifts and the drag ends. */
    fun releaseLock() { locked = null; leftUp() }

    /** Leaving the page: whatever the button is doing ends (sent before the session closes). */
    fun releaseAll(t: Long) {
        // A held-back two-finger tap was a finished tap: it still right-clicks.
        fireRight()
        cancel(t); locked = null; leftUp()
    }

    private fun showRipple(kind: RippleKind, x: Float, y: Float, feel: Boolean = true) { ripple = Ripple(kind, x, y, ++rippleSeq); if (feel) onClick() }

    companion object {
        /** 1.5 mm in dp (160 dp per inch). */
        const val SLOP_DP = 1.5f / 25.4f * 160f
        const val RELAY_DP = 6f
        const val DRAG_TICK_DP = 12f
        /** A tap's button-up waits this long for the next finger (tap-and-drag, docs §4.1). */
        const val DRAG_ARM_MS = 300L
        const val TAP_MS = 200L
        const val TWO_FINGER_TAP_MS = 300L
        /**
         * The touchpad counts as idle this long after the last finger lifted (and the drag lock let go).
         * Defers the keyboard from the computer's text focus ([PcTextFocusGate]).
         */
        const val TOUCHPAD_IDLE_DELAY_MS = 1200L
        /**
         * A two-finger tap waits this long for a second one (smart zoom) before it right-clicks (user
         * 2026-11-12: a Mac trackpad decides with force feedback; we delay instead).
         */
        const val TWO_FINGER_DOUBLE_TAP_MS = 300L
        /** Feedback only (the second ring); the target OS decides what a double click is. */
        const val DOUBLE_CLICK_MS = 500L
        /** A trackpad's contacts; more makes the touch inert. */
        const val MAX_FINGERS = 5
    }
}
