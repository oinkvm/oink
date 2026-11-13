package com.hachiwii.oink.control

/**
 * The control page's view of the core's `MobileControl` (protocol-v2.md §14.6). The page only talks
 * to this interface; the Rust adapter maps it one to one onto the uniffi object. Coordinates are in
 * dp (the host unit given to the core as `pointsPerMm`).
 */
interface ControlLink {
    fun setSettings(pointerSpeed: Int, pointsPerMm: Float)
    /**
     * One combined movement per pointer event. [timeUs] / [sinceUs]: touch times (µs of
     * `uptimeMillis`) of this movement and of the sample it is measured from (the previous movement,
     * or the touch-down for the first); the core takes the finger speed from them.
     */
    fun pointerMove(dx: Float, dy: Float, timeUs: Long, sinceUs: Long)
    fun button(button: ControlButton, down: Boolean)
    /**
     * A touchpad scroll of the fingers in dp, same direction as the fingers, with its phase, timed like
     * [pointerMove]. The core accelerates it and glides on after [PadScrollPhase.ENDED], as a Mac trackpad.
     */
    fun scroll(phase: PadScrollPhase, dx: Float, dy: Float, timeUs: Long, sinceUs: Long)
    /** Every finger on the pad after a touch event (mm from its top left), the pad's size in mm: raw contacts. */
    fun touches(timeUs: Long, width: Float, height: Float, touches: List<PadContact>, cancelled: Boolean)
    /**
     * The same frame through the core's multi-finger recognizer (shared with iOS; `MobileControl.recognize`):
     * [multiFinger] — the page leaves the touch's fingers to it. Answers at once on the calling thread and
     * sends nothing: [CoreFrame.forward] sends its outputs (`sendRecognized`), on the thread the caller runs it on.
     */
    fun recognize(timeUs: Long, touches: List<PadContact>, cancelled: Boolean, multiFinger: Boolean): CoreFrame = CoreFrame.UNCLAIMED
    /** A touchpad gesture the page decided itself (the two-finger double tap's smart zoom). */
    fun gesture(gesture: PadGesture)
    /** [code] = (HID page shl 16) or usage; modifiers are ordinary down / up. */
    fun key(code: Int, down: Boolean)
    /** Committed input-method text. Throws [ControlFailure]. */
    fun inputText(text: String)
    /** Device picker: select the computer and move the pointer there. Throws [ControlFailure]. */
    fun movePointerTo(device: String)
    fun setFocusLock(locked: Boolean)
    fun setGameMode(mode: ControlGameMode)
    /** Releases everything this phone holds down and leaves the focus session. */
    fun close()
    /**
     * What a character key of the emulated keyboard sends (docs/control-page.md §3.1.2, 调节 › 键盘布局):
     * [ch] as on the key without Shift, [layout] the setting's id, [targetMac] whether the focus
     * computer is a Mac, [mods] the modifiers down. The core's `controlTypedChar` (the layout tables
     * live in Rust, shared with iOS); it answers at once on the calling thread.
     */
    fun typedChar(layout: String, targetMac: Boolean, ch: Char, mods: Set<Mod>): TypedChar
}

/** What one character key of the emulated keyboard sends ([ControlLink.typedChar]). */
sealed interface TypedChar {
    /**
     * [code] goes down, and up when the finger lifts. [autoShift]: left Shift around just this key;
     * [autoAlt]: that key (AltGr = right Alt on Windows, Option = left Alt on a Mac) around it,
     * inside the Shift. [dead]: tapped at once and followed by a Space tap, never held or repeated.
     */
    data class Key(val code: Int, val autoShift: Boolean = false, val autoAlt: Int? = null, val dead: Boolean = false) : TypedChar
    /** The layout has no certain key for the character: it goes as committed text. */
    data class Text(val text: String) : TypedChar
}

/** One finger for the core's raw contacts, millimetres from the pad's top left. */
data class PadContact(val id: Long, val x: Float, val y: Float)

/** Updates from the core, delivered on a core thread. */
interface ControlListener {
    fun onState(state: ControlState)
    fun onEdge(edge: ControlEdge)
    /** The focus computer did not insert a text sent with [ControlLink.inputText]. */
    fun onTextRefused(reason: TextRefusal)
    /** The focus computer's text-input focus changed, or the page just opened (protocol-v2 §14.7). */
    fun onTextFocus(report: PcTextFocusReport) {}
}

enum class TextRefusal {
    /** The computer paused sharing. */
    PAUSED,
    /** The computer has not joined the focus session (its own pointer is local). */
    NOT_JOINED,
    /** A resend of a text already inserted: nothing to tell the user. */
    DUPLICATE,
    /** The target cannot insert text (a phone). */
    UNSUPPORTED;

    /** What the page shows; null when there is nothing to say. */
    val message: String? get() = when (this) {
        PAUSED -> "焦点电脑已暂停共享，文字没有输入"
        NOT_JOINED -> "焦点电脑没有加入键鼠共享，文字没有输入"
        UNSUPPORTED -> "这台设备不能输入文字"
        DUPLICATE -> null
    }
}

/** Opens a control session (`MobileSession.control`); null while the core is not running. */
fun interface ControlOpener { fun open(listener: ControlListener): ControlLink? }

/** A command the core refused, with its user-facing reason. */
class ControlFailure(message: String) : Exception(message)

enum class ControlButton { LEFT, RIGHT, MIDDLE }
enum class ControlGameMode { OFF, ON, AUTO }
enum class ControlEdgeState { CLEAR, BLOCKED, APPROACHING }
enum class ControlEdgeSide { LEFT, RIGHT, TOP, BOTTOM }

/** [platform]: "mac", "windows" or "android" (the core's "macos" is mapped by the adapter). */
data class ControlTarget(val device: String, val name: String, val platform: String, val online: Boolean)

data class ControlState(
    val focusDevice: String? = null,
    val selectedDevice: String? = null,
    /** Input has nowhere to go: no focus in the group and nothing selected. */
    val noTarget: Boolean = false,
    val locked: Boolean = false,
    val gameMode: ControlGameMode = ControlGameMode.OFF,
    val gameActive: Boolean = false,
    val targets: List<ControlTarget> = emptyList(),
) {
    /** The computer the page controls: the focus computer, else the one picked in the device list. */
    val focus: ControlTarget? get() = targets.firstOrNull { it.device == focusDevice } ?: targets.firstOrNull { it.device == selectedDevice }
}

data class ControlEdge(val state: ControlEdgeState, val edge: ControlEdgeSide, val toDevice: String?)
