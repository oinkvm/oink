package com.hachiwii.oink.control

import uniffi.oink_mobile_ffi.ControlModifiersDown
import uniffi.oink_mobile_ffi.ControlTextFocus
import uniffi.oink_mobile_ffi.ControlTextRefusal
import uniffi.oink_mobile_ffi.MobileControl
import uniffi.oink_mobile_ffi.MobileControlListener
import uniffi.oink_mobile_ffi.MobileControlSettings
import uniffi.oink_mobile_ffi.MobileControlState
import uniffi.oink_mobile_ffi.MobileException
import uniffi.oink_mobile_ffi.MobileSession
import uniffi.oink_mobile_ffi.UserFacingCode
import uniffi.oink_mobile_ffi.controlTypedChar
import uniffi.oink_mobile_ffi.ControlButton as FfiButton
import uniffi.oink_mobile_ffi.ControlEdge as FfiEdge
import uniffi.oink_mobile_ffi.ControlEdgeSide as FfiEdgeSide
import uniffi.oink_mobile_ffi.ControlEdgeState as FfiEdgeState
import uniffi.oink_mobile_ffi.ControlGameMode as FfiGameMode
import uniffi.oink_mobile_ffi.ControlTypedChar as FfiTypedChar
import uniffi.oink_mobile_ffi.ControlGesture as FfiGesture
import uniffi.oink_mobile_ffi.ControlGesturePhase as FfiGesturePhase
import uniffi.oink_mobile_ffi.ControlRecognized
import uniffi.oink_mobile_ffi.ControlScrollPhase as FfiScrollPhase
import uniffi.oink_mobile_ffi.ControlTouch
import uniffi.oink_mobile_ffi.ControlTouchClaim

/** [ControlLink] over the core's `MobileControl` (protocol-v2.md §14.6). */
class RustControl private constructor(private val control: MobileControl) : ControlLink {
    /** Refused commands become [ControlFailure] with what the page shows. */
    private inline fun command(block: () -> Unit) {
        try { block() } catch (e: MobileException) { throw ControlFailure(failureText(e)) }
    }

    override fun setSettings(pointerSpeed: Int, pointsPerMm: Float) =
        control.setSettings(MobileControlSettings(pointerSpeed.coerceIn(1, 10).toUByte(), pointsPerMm))
    override fun pointerMove(dx: Float, dy: Float, timeUs: Long, sinceUs: Long) =
        control.pointerMove(dx, dy, timeUs.coerceAtLeast(0).toULong(), sinceUs.coerceAtLeast(0).toULong())
    override fun button(button: ControlButton, down: Boolean) = control.button(when (button) {
        ControlButton.LEFT -> FfiButton.LEFT; ControlButton.RIGHT -> FfiButton.RIGHT; ControlButton.MIDDLE -> FfiButton.MIDDLE
    }, down)
    override fun scroll(phase: PadScrollPhase, dx: Float, dy: Float, timeUs: Long, sinceUs: Long) =
        control.scroll(when (phase) {
            PadScrollPhase.BEGAN -> FfiScrollPhase.BEGAN
            PadScrollPhase.CHANGED -> FfiScrollPhase.CHANGED
            PadScrollPhase.ENDED -> FfiScrollPhase.ENDED
            PadScrollPhase.CANCELLED -> FfiScrollPhase.CANCELLED
        }, dx, dy, timeUs.coerceAtLeast(0).toULong(), sinceUs.coerceAtLeast(0).toULong())
    /** Fingers in the last touches report (diagnostics: logged when the count changes). */
    private var lastFingers = 0
    override fun touches(timeUs: Long, width: Float, height: Float, touches: List<PadContact>, cancelled: Boolean) {
        if (touches.size != lastFingers || cancelled) {
            android.util.Log.i("OinkPad", "touches to core: fingers=${touches.size} pad=%.1fx%.1f mm cancelled=$cancelled ids=${touches.map { it.id }}"
                .format(width, height))
            lastFingers = touches.size
        }
        control.touches(timeUs.coerceAtLeast(0).toULong(), width, height,
            touches.map { ControlTouch(it.id.coerceAtLeast(0).toULong(), it.x, it.y) }, cancelled)
    }
    /** The core's claim in the last recognized frame (diagnostics: logged when it changes). */
    private var lastClaim = CoreClaim.UNCLAIMED
    override fun recognize(timeUs: Long, touches: List<PadContact>, cancelled: Boolean, multiFinger: Boolean): CoreFrame {
        val result = control.recognize(timeUs.coerceAtLeast(0).toULong(),
            touches.map { ControlTouch(it.id.coerceAtLeast(0).toULong(), it.x, it.y) }, cancelled, multiFinger)
        val claim = when (result.claim) {
            ControlTouchClaim.UNCLAIMED -> CoreClaim.UNCLAIMED
            ControlTouchClaim.SCROLL -> CoreClaim.SCROLL
            ControlTouchClaim.GESTURE -> CoreClaim.GESTURE
        }
        if (claim != lastClaim) {
            android.util.Log.i("OinkPad", "core claim: $lastClaim -> $claim fingers=${touches.size} multiFinger=$multiFinger")
            lastClaim = claim
        }
        // Gesture and scroll begin / end to logcat; never per change.
        for (output in result.outputs) {
            val changed = when (output) {
                is ControlRecognized.Gesture -> when (val g = output.gesture) {
                    is FfiGesture.DockSwipe -> g.phase == FfiGesturePhase.CHANGED
                    is FfiGesture.Magnify -> g.phase == FfiGesturePhase.CHANGED
                    is FfiGesture.Rotate -> g.phase == FfiGesturePhase.CHANGED
                    else -> false
                }
                is ControlRecognized.Scroll -> output.phase == FfiScrollPhase.CHANGED
            }
            if (!changed) android.util.Log.i("OinkPad", "core output: $output")
        }
        val outputs = result.outputs
        if (outputs.isEmpty()) return CoreFrame(claim)
        return CoreFrame(claim) { control.sendRecognized(outputs) }
    }
    override fun gesture(gesture: PadGesture) {
        android.util.Log.i("OinkPad", "gesture to core: $gesture")
        control.gesture(when (gesture) {
            PadGesture.SmartMagnify -> FfiGesture.SmartMagnify
        })
    }
    override fun key(code: Int, down: Boolean) = control.key(code.toUInt(), down)
    override fun inputText(text: String) = command { control.inputText(text) }
    override fun movePointerTo(device: String) = command { control.movePointerTo(device) }
    override fun setFocusLock(locked: Boolean) = command { control.setFocusLock(locked) }
    override fun setGameMode(mode: ControlGameMode) = command {
        control.setGameMode(when (mode) { ControlGameMode.OFF -> FfiGameMode.OFF; ControlGameMode.ON -> FfiGameMode.ON; ControlGameMode.AUTO -> FfiGameMode.AUTO })
    }
    /** `leave()` releases what the phone holds; `close()` then frees the native handle. */
    override fun close() { control.leave(); control.close() }
    override fun typedChar(layout: String, targetMac: Boolean, ch: Char, mods: Set<Mod>): TypedChar =
        when (val t = controlTypedChar(layout, targetMac, ch.toString(),
            ControlModifiersDown(Mod.SHIFT in mods, Mod.CTRL in mods, Mod.ALT in mods, Mod.META in mods))) {
            is FfiTypedChar.Key -> TypedChar.Key(t.code.toInt(), t.autoShift, t.autoAlt?.toInt(), t.dead)
            is FfiTypedChar.Text -> TypedChar.Text(t.text)
        }

    companion object {
        /** Opens the page's handle on a running session (a newer one replaces the previous). */
        fun open(session: MobileSession, listener: ControlListener): ControlLink = RustControl(session.control(object : MobileControlListener {
            override fun onState(state: MobileControlState) = listener.onState(state.toUi())
            override fun onEdge(edge: FfiEdge) = listener.onEdge(edge.toUi())
            override fun onTextRefused(reason: ControlTextRefusal) = listener.onTextRefused(when (reason) {
                ControlTextRefusal.PAUSED -> TextRefusal.PAUSED
                ControlTextRefusal.NOT_JOINED -> TextRefusal.NOT_JOINED
                ControlTextRefusal.DUPLICATE -> TextRefusal.DUPLICATE
                ControlTextRefusal.UNSUPPORTED -> TextRefusal.UNSUPPORTED
            })
            override fun onTextFocus(device: String, focus: ControlTextFocus) =
                listener.onTextFocus(PcTextFocusReport(device, focus.seq.toLong(), focus.editable, focus.secure))
        }))

        internal fun failureText(e: MobileException): String = when (e) {
            is MobileException.Command -> when (e.code) {
                UserFacingCode.DEVICE_UNAVAILABLE -> "请先选择要控制的电脑"
                UserFacingCode.LINK_LOST -> "与电脑的连接中断，请重试"
                UserFacingCode.STATE_CONFLICT -> "键鼠共享已暂停，输入没有发出"
                UserFacingCode.UNSUPPORTED -> "这台设备不支持这项操作"
                else -> e.detail
            }
            is MobileException.Stopped -> "Oink 没有在运行"
            else -> e.message ?: "操作失败"
        }

        fun MobileControlState.toUi() = ControlState(
            focusDevice = focusDevice, selectedDevice = selectedDevice, noTarget = noTarget, locked = locked,
            gameMode = when (gameMode) { FfiGameMode.OFF -> ControlGameMode.OFF; FfiGameMode.ON -> ControlGameMode.ON; FfiGameMode.AUTO -> ControlGameMode.AUTO },
            gameActive = gameActive,
            // The core names the Mac "macos"; the page's platform icons use "mac".
            targets = targets.map { ControlTarget(it.device, it.name, if (it.platform == "macos") "mac" else it.platform, it.online) },
        )

        private fun FfiEdge.toUi() = ControlEdge(
            when (state) { FfiEdgeState.CLEAR -> ControlEdgeState.CLEAR; FfiEdgeState.BLOCKED -> ControlEdgeState.BLOCKED; FfiEdgeState.APPROACHING -> ControlEdgeState.APPROACHING },
            when (edge) { FfiEdgeSide.LEFT -> ControlEdgeSide.LEFT; FfiEdgeSide.RIGHT -> ControlEdgeSide.RIGHT; FfiEdgeSide.TOP -> ControlEdgeSide.TOP; FfiEdgeSide.BOTTOM -> ControlEdgeSide.BOTTOM },
            toDevice)
    }
}
