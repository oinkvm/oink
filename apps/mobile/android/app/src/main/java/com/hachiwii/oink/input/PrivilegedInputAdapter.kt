package com.hachiwii.oink.input

import android.os.Bundle
import com.hachiwii.oink.privilege.PrivilegeClient
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import uniffi.oink_mobile_ffi.*
import java.nio.ByteBuffer
import java.util.UUID

/** How one helper reply affects the input lane (review M38). */
internal enum class HelperReply { OK, UNAVAILABLE, FATAL }

/**
 * `locked`, a stale display generation, a missing pointer or a point outside the display are
 * moments of the device, not a broken helper: that command fails and input waits for the next
 * helper event (unlock, display, input state). Only a failed or refused helper call disables it.
 */
internal fun helperReply(status: String?): HelperReply = when (status) {
    "ok", "pointer", "keyboard_pending" -> HelperReply.OK
    "locked", "stale_display", "pointer_unavailable", "outside_display", "coordinate_unavailable", "owner_inactive" ->
        HelperReply.UNAVAILABLE
    else -> HelperReply.FATAL
}

/** The helper cannot take input at this moment; see [helperReply]. */
internal class HelperUnavailable(val status: String) : Exception("helper input unavailable ($status)")

/** Ordered helper lane. Acknowledgements fence native releases before a new I0 pair attaches. */
class PrivilegedInputAdapter(
    private val failed: (Exception, String?) -> Unit,
    private val readiness: (Boolean) -> Unit,
) {
    private val controller = AndroidInputController()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val commands = Channel<suspend () -> Unit>(Channel.UNLIMITED)
    private var display: AndroidDisplayState? = null
    private var incarnation: String? = null
    private var token = ByteArray(16)
    private var buttons = 0
    private var focused = false
    private var pendingLogged = false
    private var target = AndroidPointerPoint(0.0, 0.0)

    init {
        scope.launch {
            for (command in commands) {
                try { command() }
                catch (e: CancellationException) { throw e }
                catch (e: HelperUnavailable) {
                    // This command failed (its ACK says so and the core releases the focus).
                    // The helper stays enabled; its next event (unlock, display, input state)
                    // reconfigures input. Readiness is not touched here: it still drives the
                    // negotiated capability, and changing that would drop every link.
                    android.util.Log.i("OinkInput", "helper_unavailable status=${e.status}")
                    controller.cancel(); focused = false; buttons = 0
                }
                catch (e: Exception) {
                    // Detection only: this turns privileged input off, so say which call and why.
                    android.util.Log.w("OinkInput", "input lane failed; disabling the helper", e)
                    controller.cancel(); display = null; focused = false
                    // Disable also closes the pointer if releasing UHID
                    // failed midway. Never disable a replacement helper.
                    try { PrivilegeClient.enable(false, incarnation) } catch (_: Exception) { }
                    failed(e, incarnation)
                }
            }
        }
    }
    private fun enqueue(command: suspend () -> Unit) { commands.trySend(command) }
    private fun request(op: String, fields: Bundle.() -> Unit = {}): Bundle {
        val reply = PrivilegeClient.request(Bundle().apply { putString("op", op); fields() }, incarnation)
        val status = reply.getString("status")
        when (helperReply(status)) {
            HelperReply.OK -> {}
            HelperReply.UNAVAILABLE -> throw HelperUnavailable(status.orEmpty())
            HelperReply.FATAL -> error("helper $op failed ($status ${reply.getString("error").orEmpty()})")
        }
        return reply
    }
    fun configure(value: AndroidDisplayState?, instance: String? = null) {
        enqueue {
            val next = instance ?: incarnation
            if (next == incarnation) release()
            else {
                controller.cancel(); focused = false; buttons = 0
            }
            incarnation = next
            token = next?.let { UUID.fromString(it) }?.let {
                ByteBuffer.allocate(16).putLong(it.mostSignificantBits).putLong(it.leastSignificantBits).array()
            } ?: ByteArray(16)
            display = value
            controller.display(value)
        }
    }
    fun apply(command: AndroidInputCommand, acknowledge: suspend (Boolean) -> Unit) {
        // Capture the deadline before queuing Binder work. A slow pointer/scroll lane
        // must not turn an already stale activity callback into a new 3-second lease.
        val activityUntil = android.os.SystemClock.elapsedRealtime() + 3000L
        if (!commands.trySend {
            var success = false
            try {
                val action = command.action
                val current = command.helperIncarnation.contentEquals(token)
                if (action is AndroidInputAction.Ready) {
                    readiness(action.ready && current && display?.generation == command.generation)
                } else if (current && (action is AndroidInputAction.ReleaseAll || display?.generation == command.generation)) {
                    when (action) {
                        is AndroidInputAction.Focus -> focus(action.active)
                        is AndroidInputAction.Move -> if (focused) pointer(action.x, action.y, command.generation)
                        is AndroidInputAction.Key -> if (focused) key(action.page, action.usage, action.pressed)
                        is AndroidInputAction.Button -> if (focused) {
                            val bit = 1 shl action.button.toInt()
                            buttonsAndWheel(if (action.pressed) buttons or bit else buttons and bit.inv(), 0f, 0f)
                        }
                        is AndroidInputAction.Scroll -> if (focused) scroll(action.horizontal, action.vertical)
                        is AndroidInputAction.SystemGesture -> if (focused) request("system_gesture") {
                            putString("gesture", when (action.gesture) {
                                AndroidSystemGesture.HOME -> "home"
                                AndroidSystemGesture.RECENTS -> "recents"
                                AndroidSystemGesture.SWITCH_NEXT -> "switch_next"
                                AndroidSystemGesture.SWITCH_PREVIOUS -> "switch_previous"
                                AndroidSystemGesture.NOTIFICATIONS -> "notifications"
                            })
                            putLong("display_generation", command.generation.toLong())
                        }
                        is AndroidInputAction.GroupActivity -> request("group_activity") {
                            putLong("until_elapsed_ms", if (action.active) activityUntil else 0L)
                        }
                        is AndroidInputAction.ReleaseAll -> release()
                        else -> {}
                    }
                }
                // Old helper/display commands are retired, including their ACKs.
                success = true
            } finally { acknowledge(success) }
        }.isSuccess) scope.launch { acknowledge(false) }
    }
    private fun focus(active: Boolean) {
        if (!active) { release(); return }
        if (display == null) return
        controller.focus(true)
        focused = true
        pendingLogged = false
        request("keyboard_focus") { putBoolean("active", true) }
    }
    fun event(event: Bundle) = enqueue {
        if (event.getString("incarnation") != incarnation) return@enqueue
        when (event.getString("status")) {
            "keyboard_device_ready" -> controller.keyboardReady(true)
            "keyboard_removed" -> controller.keyboardReady(false)
            "ipc_lost" -> { controller.cancel(); display = null }
        }
    }
    private fun key(page: UShort, usage: UShort, pressed: Boolean) {
        val report = controller.key(page, usage, pressed)
        if (report.pending) {
            if (!pendingLogged) { android.util.Log.i("OinkInput", "keyboard_pending"); pendingLogged = true }
        } else if (!report.unsupported) {
            request("keyboard") {
                putInt("modifiers", report.modifiers.toInt())
                putByteArray("usages", report.usages)
            }
            if (report.rollover) android.util.Log.w("OinkInput", "keyboard_rollover")
        }
    }
    private fun pointer(x: Double, y: Double, generation: ULong) {
        val facts = display ?: return
        if (facts.generation != generation || facts.width == 0u || facts.height == 0u) return
        // Router bounds use an exclusive f64 right edge. Rounding width-epsilon
        // to Android float would produce width and fail absolute injection.
        val physical = AndroidPointerPoint(x.coerceIn(0.0, facts.width.toDouble() - 1.0),
            y.coerceIn(0.0, facts.height.toDouble() - 1.0))
        if (!controller.target(physical, generation)) return
        target = physical
        request("mouse_absolute") {
            putFloat("x", target.x.toFloat()); putFloat("y", target.y.toFloat())
            putLong("display_generation", generation.toLong()); putInt("buttons", buttons)
        }
    }
    private fun scroll(horizontal: Double, vertical: Double) {
        require(horizontal.isFinite() && vertical.isFinite())
        // ACTION_SCROLL accepts fractional axes directly. Splitting into integer wheel
        // reports loses touchpad precision in VirtualMouse's native uinput conversion.
        buttonsAndWheel(buttons, vertical.toFloat(), horizontal.toFloat())
    }
    private fun buttonsAndWheel(mask: Int, wheel: Float, horizontal: Float) {
        val facts = display ?: return
        buttons = mask
        request("mouse_absolute") {
            putFloat("x", target.x.toFloat()); putFloat("y", target.y.toFloat())
            putInt("buttons", buttons); putFloat("wheel", wheel); putFloat("hwheel", horizontal)
            putLong("display_generation", facts.generation.toLong())
        }
    }
    private fun release() {
        val hadInput = display != null || focused || buttons != 0
        controller.cancel(); buttons = 0; focused = false
        if (hadInput && PrivilegeClient.available()) {
            try { request("release_all") }
            finally { request("keyboard_focus") { putBoolean("active", false) } }
        }
    }
    suspend fun close() {
        val released = CompletableDeferred<Unit>()
        enqueue { try { release() } finally { released.complete(Unit) } }
        released.await()
        commands.close()
        scope.coroutineContext[Job]?.cancelAndJoin()
        controller.destroy()
    }
}
