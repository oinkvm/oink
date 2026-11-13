package com.hachiwii.oink.control

import android.view.KeyEvent

/** Key codes as the core takes them: (HID usage page shl 16) or usage. */
object Hid {
    private const val KEYBOARD = 0x07 shl 16
    private const val CONSUMER = 0x0C shl 16
    fun key(usage: Int) = KEYBOARD or usage
    fun consumer(usage: Int) = CONSUMER or usage

    val A = key(0x04)
    val RETURN = key(0x28); val ESCAPE = key(0x29); val BACKSPACE = key(0x2A); val TAB = key(0x2B); val SPACE = key(0x2C)
    val F1 = key(0x3A)
    fun f(n: Int) = key(0x3A + n - 1)
    val HOME = key(0x4A); val PAGE_UP = key(0x4B); val DELETE = key(0x4C); val END = key(0x4D); val PAGE_DOWN = key(0x4E)
    val RIGHT = key(0x4F); val LEFT = key(0x50); val DOWN = key(0x51); val UP = key(0x52)
    val L_CTRL = key(0xE0); val L_SHIFT = key(0xE1); val L_ALT = key(0xE2); val L_GUI = key(0xE3)
    val R_CTRL = key(0xE4); val R_SHIFT = key(0xE5); val R_ALT = key(0xE6); val R_GUI = key(0xE7)
    val C = key(0x06); val V = key(0x19)
    val CAPS_LOCK = key(0x39); val PRINT_SCREEN = key(0x46)

    val PLAY_PAUSE = consumer(0xCD); val NEXT = consumer(0xB5); val PREVIOUS = consumer(0xB6)
    val MUTE = consumer(0xE2); val VOLUME_UP = consumer(0xE9); val VOLUME_DOWN = consumer(0xEA)

    /**
     * Keys that leave the focus computer's text and caret as they are: modifiers, Caps Lock and the
     * media (consumer) keys. Any other key may type, delete or move the caret there.
     */
    fun keepsCaret(code: Int) = code ushr 16 != KEYBOARD ushr 16 || code in L_CTRL..R_GUI || code == CAPS_LOCK

    /** What a base key types with Shift down on a US layout (labels of the symbol layer while Shift is down). */
    val US_SHIFT_LABEL = mapOf('`' to '~', '1' to '!', '2' to '@', '3' to '#', '4' to '$', '5' to '%', '6' to '^', '7' to '&', '8' to '*',
        '9' to '(', '0' to ')', '-' to '_', '=' to '+', '[' to '{', ']' to '}', '\\' to '|', ';' to ':', '\'' to '"',
        ',' to '<', '.' to '>', '/' to '?')

    /** A hardware keyboard key (Android key code) as a HID code; null for keys with no HID equivalent. */
    fun fromAndroid(keyCode: Int): Int? = when (keyCode) {
        in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z -> key(0x04 + keyCode - KeyEvent.KEYCODE_A)
        in KeyEvent.KEYCODE_1..KeyEvent.KEYCODE_9 -> key(0x1E + keyCode - KeyEvent.KEYCODE_1)
        KeyEvent.KEYCODE_0 -> key(0x27)
        in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 -> key(0x3A + keyCode - KeyEvent.KEYCODE_F1)
        KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> RETURN
        KeyEvent.KEYCODE_ESCAPE -> ESCAPE
        KeyEvent.KEYCODE_DEL -> BACKSPACE
        KeyEvent.KEYCODE_FORWARD_DEL -> DELETE
        KeyEvent.KEYCODE_TAB -> TAB
        KeyEvent.KEYCODE_SPACE -> SPACE
        KeyEvent.KEYCODE_MINUS -> key(0x2D)
        KeyEvent.KEYCODE_EQUALS -> key(0x2E)
        KeyEvent.KEYCODE_LEFT_BRACKET -> key(0x2F)
        KeyEvent.KEYCODE_RIGHT_BRACKET -> key(0x30)
        KeyEvent.KEYCODE_BACKSLASH -> key(0x31)
        KeyEvent.KEYCODE_SEMICOLON -> key(0x33)
        KeyEvent.KEYCODE_APOSTROPHE -> key(0x34)
        KeyEvent.KEYCODE_GRAVE -> key(0x35)
        KeyEvent.KEYCODE_COMMA -> key(0x36)
        KeyEvent.KEYCODE_PERIOD -> key(0x37)
        KeyEvent.KEYCODE_SLASH -> key(0x38)
        KeyEvent.KEYCODE_CAPS_LOCK -> CAPS_LOCK
        KeyEvent.KEYCODE_SYSRQ -> PRINT_SCREEN
        KeyEvent.KEYCODE_SCROLL_LOCK -> key(0x47)
        KeyEvent.KEYCODE_BREAK -> key(0x48)
        KeyEvent.KEYCODE_INSERT -> key(0x49)
        KeyEvent.KEYCODE_MOVE_HOME -> HOME
        KeyEvent.KEYCODE_PAGE_UP -> PAGE_UP
        KeyEvent.KEYCODE_MOVE_END -> END
        KeyEvent.KEYCODE_PAGE_DOWN -> PAGE_DOWN
        KeyEvent.KEYCODE_DPAD_RIGHT -> RIGHT
        KeyEvent.KEYCODE_DPAD_LEFT -> LEFT
        KeyEvent.KEYCODE_DPAD_DOWN -> DOWN
        KeyEvent.KEYCODE_DPAD_UP -> UP
        KeyEvent.KEYCODE_NUM_LOCK -> key(0x53)
        KeyEvent.KEYCODE_MENU -> key(0x65)
        KeyEvent.KEYCODE_CTRL_LEFT -> L_CTRL
        KeyEvent.KEYCODE_SHIFT_LEFT -> L_SHIFT
        KeyEvent.KEYCODE_ALT_LEFT -> L_ALT
        KeyEvent.KEYCODE_META_LEFT -> L_GUI
        KeyEvent.KEYCODE_CTRL_RIGHT -> R_CTRL
        KeyEvent.KEYCODE_SHIFT_RIGHT -> R_SHIFT
        KeyEvent.KEYCODE_ALT_RIGHT -> R_ALT
        KeyEvent.KEYCODE_META_RIGHT -> R_GUI
        KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE -> PLAY_PAUSE
        KeyEvent.KEYCODE_MEDIA_NEXT -> NEXT
        KeyEvent.KEYCODE_MEDIA_PREVIOUS -> PREVIOUS
        KeyEvent.KEYCODE_VOLUME_MUTE -> MUTE
        else -> null
    }
}
