package com.hachiwii.oink.control

import android.icu.text.BreakIterator

/** One piece of committed text as it goes out: an `InputText`, or a key for a line break / tab. */
sealed interface TextPart {
    data class Text(val text: String) : TextPart
    data class Key(val code: Int) : TextPart
}

/**
 * Splits committed input-method text for `inputText` (protocol-v2.md §14.6): one call carries at most
 * [MAX_BYTES] UTF-8 bytes and no control characters. Line breaks become Return and tabs Tab (in the
 * same ordered stream); other control characters are dropped. The core sends one call whole or not
 * at all, so a cut never splits a character: long text is cut between (ICU, extended) grapheme
 * clusters, so an emoji or a combined character is never split across two messages.
 */
object TextParts {
    const val MAX_BYTES = 4096

    fun split(text: String): List<TextPart> {
        val out = ArrayList<TextPart>()
        val run = StringBuilder()
        fun flush() { if (run.isNotEmpty()) { chunk(run.toString(), out); run.clear() } }
        var i = 0
        while (i < text.length) {
            val ch = text[i]
            when {
                ch == '\r' && text.getOrNull(i + 1) == '\n' -> { flush(); out += TextPart.Key(Hid.RETURN); i++ }
                ch == '\n' || ch == '\r' || ch == ' ' || ch == ' ' -> { flush(); out += TextPart.Key(Hid.RETURN) }
                ch == '\t' -> { flush(); out += TextPart.Key(Hid.TAB) }
                Character.isISOControl(ch) -> {}
                else -> run.append(ch)
            }
            i++
        }
        flush()
        return out
    }

    private fun chunk(text: String, out: MutableList<TextPart>) {
        if (text.toByteArray(Charsets.UTF_8).size <= MAX_BYTES) { out += TextPart.Text(text); return }
        // android.icu, not java.text: java.text's character iterator only follows extended grapheme
        // clusters on some runtimes (JDK 17 splits 👍 from its skin tone), ICU does everywhere.
        val it = BreakIterator.getCharacterInstance().apply { setText(text) }
        var start = 0; var bytes = 0
        var prev = it.first()
        var next = it.next()
        while (next != BreakIterator.DONE) {
            val n = text.substring(prev, next).toByteArray(Charsets.UTF_8).size
            if (bytes + n > MAX_BYTES && prev > start) {
                out += TextPart.Text(text.substring(start, prev)); start = prev; bytes = 0
            }
            bytes += n
            prev = next; next = it.next()
        }
        if (start < text.length) out += TextPart.Text(text.substring(start))
    }
}
