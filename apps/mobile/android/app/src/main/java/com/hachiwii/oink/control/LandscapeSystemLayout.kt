package com.hachiwii.oink.control

/** System IME layout. Remaining height includes the status bar; drawing excludes its inset. */
object LandscapeSystemLayout {
    const val REFERENCE_WIDTH = 405f
    const val SPLIT_HEIGHT = REFERENCE_WIDTH / 3f
    fun split(remainingHeight: Float) = remainingHeight >= SPLIT_HEIGHT

    fun rows(remainingHeight: Float, fn: Boolean, mac: Boolean): List<List<String>> {
        val split = split(remainingHeight)
        val media = listOf("prev", "play", "next", "mute", "vol-", "vol+")
        val navigation = listOf("home", "end", "pgup", "pgdn")
        val mods = if (mac) listOf("ctrl", "alt", "meta") else listOf("ctrl", "meta", "alt")
        val middle = if (fn) (1..12).map { "f$it" }
            else listOf("copy", "paste") + (if (split) emptyList() else navigation) + media
        val top = listOf("esc") + middle + "switch"
        return if (split) listOf(top, listOf("caps") + navigation + listOf("fn", "up", "hide"),
            mods + listOf("tab", "del", "left", "down", "right"))
        else listOf(top, listOf("caps") + mods + listOf("tab", "del", "fn", "left", "up", "down", "right", "hide"))
    }
}
