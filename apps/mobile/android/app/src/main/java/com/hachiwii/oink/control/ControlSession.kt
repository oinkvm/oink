package com.hachiwii.oink.control

import android.os.Handler
import android.os.Looper
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import java.util.concurrent.Executor
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

/**
 * One open control session. Commands run in order on one worker thread (text, then Return, keep
 * their order; the UI never waits on the core); core updates are moved to the main thread as
 * Compose state. [onFailure] gets the user-facing reason of a refused command; [textFocused] gets
 * the focus computer's text-input focus on every change and when the session opens (protocol-v2 §14.7).
 */
class ControlSession(
    opener: ControlOpener,
    private val onFailure: (String) -> Unit,
    private val worker: Executor = defaultWorker(),
    private val main: Executor = mainExecutor(),
    private val textFocused: (PcTextFocusReport) -> Unit = {},
) : PadSink {
    var state by mutableStateOf(ControlState()); private set
    var edgeReport by mutableStateOf<ControlEdge?>(null); private set
    @Volatile private var closed = false

    private val link: ControlLink? = opener.open(object : ControlListener {
        override fun onState(state: ControlState) = main.execute { if (!closed) this@ControlSession.state = state }
        override fun onEdge(edge: ControlEdge) = main.execute { if (!closed) edgeReport = edge }
        override fun onTextRefused(reason: TextRefusal) { reason.message?.let { m -> main.execute { if (!closed) onFailure(m) } } }
        override fun onTextFocus(report: PcTextFocusReport) = main.execute { if (!closed) textFocused(report) }
    })
    val available get() = link != null

    private fun run(block: ControlLink.() -> Unit) {
        val l = link ?: return
        worker.execute {
            try { l.block() } catch (e: ControlFailure) { main.execute { if (!closed) onFailure(e.message ?: "") } }
        }
    }

    /** dp per millimetre, from the last [settings]; the touches need it. */
    @Volatile private var pointsPerMm = 0f
    fun settings(speed: Int, pointsPerMm: Float) {
        this.pointsPerMm = pointsPerMm
        run { setSettings(speed, pointsPerMm) }
    }
    override fun move(dx: Float, dy: Float, t: Long, since: Long) = run { pointerMove(dx, dy, t * 1000, since * 1000) }
    override fun scroll(dx: Float, dy: Float, phase: PadScrollPhase, t: Long, since: Long) =
        run { this.scroll(phase, dx, dy, t * 1000, since * 1000) }
    override fun gesture(gesture: PadGesture) = run { this.gesture(gesture) }
    /**
     * The fingers as the trackpad's raw contacts, in millimetres (the pad's dp over [pointsPerMm]), and through
     * the core's multi-finger recognizer, which answers here at once; its outputs ([CoreFrame.forward]) queue
     * behind what the page sends for the same event.
     */
    override fun touches(t: Long, fingers: List<PadFinger>, width: Float, height: Float, cancelled: Boolean, multiFinger: Boolean): CoreFrame {
        val mm = pointsPerMm
        if (mm <= 0f) return CoreFrame.UNCLAIMED
        val list = fingers.map { PadContact(it.id, it.x / mm, it.y / mm) }
        run { this.touches(t * 1000, width / mm, height / mm, list, cancelled) }
        val frame = link?.recognize(t * 1000, list, cancelled, multiFinger) ?: return CoreFrame.UNCLAIMED
        return CoreFrame(frame.claim) { run { frame.forward() } }
    }
    override fun button(button: ControlButton, down: Boolean) { val which = button; run { this.button(which, down) } }
    fun key(code: Int, down: Boolean) = run { this.key(code, down) }
    /** Committed text, cut to the core's limits; line breaks and tabs go as keys in the same order. */
    /** A refused piece stops the rest, so nothing after it arrives out of order. */
    fun text(text: String) {
        val parts = TextParts.split(text)
        if (parts.isNotEmpty()) run {
            parts.forEach { part ->
                when (part) {
                    is TextPart.Text -> inputText(part.text)
                    is TextPart.Key -> { this.key(part.code, true); this.key(part.code, false) }
                }
            }
        }
    }
    /** What a character key sends ([ControlLink.typedChar]); answered here on the caller's thread, not queued. */
    fun typedChar(layout: String, targetMac: Boolean, ch: Char, mods: Set<Mod>): TypedChar? = link?.typedChar(layout, targetMac, ch, mods)
    fun select(device: String) = run { movePointerTo(device) }
    fun lock(locked: Boolean) = run { setFocusLock(locked) }
    fun game(mode: ControlGameMode) = run { setGameMode(mode) }

    fun close() {
        if (closed) return
        closed = true
        run { this.close() }
        (worker as? ExecutorService)?.shutdown()
    }

    companion object {
        private fun defaultWorker(): Executor = Executors.newSingleThreadExecutor { Thread(it, "oink-control").apply { isDaemon = true } }
        private fun mainExecutor(): Executor { val h = Handler(Looper.getMainLooper()); return Executor { h.post(it) } }
    }
}
