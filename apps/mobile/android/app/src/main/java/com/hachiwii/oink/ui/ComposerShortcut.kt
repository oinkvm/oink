package com.hachiwii.oink.ui

import android.view.KeyEvent
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.key.onPreviewKeyEvent

/** Inspect keys and modifiers, not their KeyCharacterMap character: Ctrl+Enter can have no
 * printable character. Meta+Enter belongs to Android's HOME policy, before app dispatch. */
internal fun composerSendShortcut(event: KeyEvent, send: () -> Unit): Boolean {
    if (event.keyCode != KeyEvent.KEYCODE_ENTER && event.keyCode != KeyEvent.KEYCODE_NUMPAD_ENTER) return false
    if (!event.hasModifiers(KeyEvent.META_CTRL_ON)) return false
    if (event.action != KeyEvent.ACTION_DOWN && event.action != KeyEvent.ACTION_UP) return false
    if (event.action == KeyEvent.ACTION_DOWN && event.repeatCount == 0) send()
    // Consume repeats and UP too, so a shortcut does not also insert a newline.
    return true
}

internal fun Modifier.composerSendShortcut(send: () -> Unit): Modifier =
    onPreviewKeyEvent { composerSendShortcut(it.nativeKeyEvent, send) }
