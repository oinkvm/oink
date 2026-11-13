package com.hachiwii.oink.control

/**
 * A US-layout stand-in for the core's `controlTypedChar` ([ControlLink.typedChar]) in JVM tests,
 * where the native core is not loaded. The layout tables themselves (US, UK, DE, FR, ES, JIS on
 * Windows and macOS) are tested in Rust (oink-mobile-core `key_layouts`). Same rules for US: the
 * face the key shows, its US key with Shift around it when needed, text when there is no key.
 */
object UsKeys {
    /** A character as a US-layout physical key: the code and whether it needs Shift. */
    fun usChar(ch: Char): Pair<Int, Boolean>? {
        if (ch in 'a'..'z') return Hid.key(0x04 + (ch - 'a')) to false
        if (ch in 'A'..'Z') return Hid.key(0x04 + (ch - 'A')) to true
        if (ch in '1'..'9') return Hid.key(0x1E + (ch - '1')) to false
        if (ch == '0') return Hid.key(0x27) to false
        PLAIN[ch]?.let { return Hid.key(it) to false }
        SHIFTED[ch]?.let { return Hid.key(it) to true }
        return null
    }

    /** [ControlLink.typedChar] for every layout and system as if it were US. */
    @Suppress("UNUSED_PARAMETER")
    fun typed(layout: String, targetMac: Boolean, ch: Char, mods: Set<Mod>): TypedChar {
        val shift = Mod.SHIFT in mods
        val face = when {
            !shift -> ch
            ch.isLetter() -> ch.uppercaseChar()
            else -> Hid.US_SHIFT_LABEL[ch] ?: ch
        }
        var key = usChar(face)?.takeIf { !shift || it.second }
        if (key == null && mods.any { it != Mod.SHIFT }) key = usChar(ch)
        return key?.let { (code, needsShift) -> TypedChar.Key(code, autoShift = needsShift && !shift) } ?: TypedChar.Text(face.toString())
    }

    private val PLAIN = mapOf(' ' to 0x2C, '-' to 0x2D, '=' to 0x2E, '[' to 0x2F, ']' to 0x30, '\\' to 0x31, ';' to 0x33,
        '\'' to 0x34, '`' to 0x35, ',' to 0x36, '.' to 0x37, '/' to 0x38)
    private val SHIFTED = mapOf('!' to 0x1E, '@' to 0x1F, '#' to 0x20, '$' to 0x21, '%' to 0x22, '^' to 0x23, '&' to 0x24,
        '*' to 0x25, '(' to 0x26, ')' to 0x27, '_' to 0x2D, '+' to 0x2E, '{' to 0x2F, '}' to 0x30, '|' to 0x31, ':' to 0x33,
        '"' to 0x34, '~' to 0x35, '<' to 0x36, '>' to 0x37, '?' to 0x38)
}
