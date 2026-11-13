package com.hachiwii.oink

/** The same off / exit / forward rule for the toolbar and the virtual keyboard. */
enum class SpeakerTap(val label: String) { EXIT("设为出口"), FORWARD("转发音频") }
internal enum class SpeakerAction { OFF, EXIT, FORWARD, REJECT }
internal fun speakerAction(speaker: Boolean, forward: Boolean, privileged: Boolean,
                           tap: SpeakerTap, long: Boolean): SpeakerAction {
    if (!long && (speaker || forward)) return SpeakerAction.OFF
    val effectiveTap = if (privileged) tap else SpeakerTap.EXIT
    val wantsForward = when {
        forward -> false
        speaker -> true
        long -> effectiveTap == SpeakerTap.EXIT
        else -> effectiveTap == SpeakerTap.FORWARD
    }
    return if (wantsForward) {
        if (privileged) SpeakerAction.FORWARD else SpeakerAction.REJECT
    } else SpeakerAction.EXIT
}
