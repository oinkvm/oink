package com.hachiwii.oink.ui

import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.displayCutout
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.systemBars
import androidx.compose.foundation.layout.union
import androidx.compose.runtime.Composable
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.layout
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.offset
import kotlin.properties.ReadWriteProperty
import kotlin.reflect.KProperty

/*
 * The soft keyboard and the app shell.
 *
 * MainActivity is edge to edge with adjustNothing: the window never resizes, the layout follows
 * the IME inset itself. That inset moves frame by frame with the keyboard's own open / close
 * animation, so everything that makes room for the keyboard reads it continuously ([ImeLift])
 * instead of switching on "is the keyboard visible" (that flag only flips at one end of the
 * animation, which made the tab bar and the composer jump).
 *
 * Overlays (menus, sheets, the image preview, settings) are drawn in the same window and focus tree
 * as the page, so opening one leaves a focused text field focused and its keyboard up. Whatever
 * covers the page therefore lives in a [Cover]: opening it clears focus first, and the keyboard
 * closes with its normal animation.
 */

/**
 * How far the keyboard currently reaches above the bottom system bars / cutout, in px: 0 while it
 * is closed, its height above the navigation bar once open, and every value in between while it
 * animates. It reads snapshot state, so read it in layout or drawing: the animation then only
 * re-lays out, nothing recomposes per frame.
 */
internal fun interface ImeLift { fun px(): Int }

/** Tests supply the keyboard's position here; null reads the window's IME inset. */
internal val LocalImeLift = compositionLocalOf<ImeLift?> { null }

@Composable
internal fun imeLift(): ImeLift {
    LocalImeLift.current?.let { return it }
    val ime = WindowInsets.ime
    val bars = WindowInsets.systemBars.union(WindowInsets.displayCutout)
    val density = LocalDensity.current
    return remember(ime, bars, density) { ImeLift { (ime.getBottom(density) - bars.getBottom(density)).coerceAtLeast(0) } }
}

/** Bottom padding of the keyboard's current [ImeLift] (the part of safeDrawing the system bars do not already cover). */
internal fun Modifier.imeLiftPadding(lift: ImeLift) = layout { measurable, constraints ->
    val pad = lift.px()
    val p = measurable.measure(constraints.offset(vertical = -pad))
    layout(p.width, (p.height + pad).coerceAtMost(constraints.maxHeight)) { p.place(0, 0) }
}

/**
 * Bottom padding of [space] less the keyboard's current [ImeLift] (never below 0): room for
 * something the keyboard slides over, such as the floating tab bar. Content above it stays put
 * until the rising keyboard reaches it and then rides on the keyboard; closing reverses that.
 */
internal fun Modifier.spaceAboveKeyboard(space: Dp, lift: ImeLift) = layout { measurable, constraints ->
    val pad = (space.roundToPx() - lift.px()).coerceAtLeast(0)
    val p = measurable.measure(constraints.offset(vertical = -pad))
    layout(p.width, (p.height + pad).coerceAtMost(constraints.maxHeight)) { p.place(0, 0) }
}

/**
 * Taps that nothing inside handles (the message list's background, gaps between bubbles) clear
 * focus, so the keyboard closes with its own animation. Children handle their taps first and
 * consume them, and a drag that scrolls cancels the tap, so buttons, messages and scrolling work
 * as before.
 */
@Composable
internal fun Modifier.dismissKeyboardOnTap(): Modifier {
    val focus = LocalFocusManager.current
    return pointerInput(focus) { detectTapGestures(onTap = { focus.clearFocus(force = true) }) }
}

/** Something that covers the page is open: not null, not false, not an empty collection. */
internal fun coverOpen(value: Any?) = when (value) {
    null, false -> false
    is Collection<*> -> value.isNotEmpty()
    else -> true
}

/**
 * State of something that covers the page (a menu, sheet, overlay page, the image preview). Setting
 * it so that it [opens] first calls [dismiss], which clears focus: a focused text field's keyboard
 * closes with the IME animation instead of staying up under the cover (or being cut off when a
 * window takes focus). It is cleared at the event, before the overlay composes, so an overlay that
 * focuses a field of its own still gets the keyboard.
 */
internal class Cover<T>(initial: T, private val opens: (T, T) -> Boolean = { old, new -> !coverOpen(old) && coverOpen(new) },
                        private val dismiss: () -> Unit) : ReadWriteProperty<Any?, T> {
    private var value by mutableStateOf(initial)
    override fun getValue(thisRef: Any?, property: KProperty<*>): T = value
    override fun setValue(thisRef: Any?, property: KProperty<*>, value: T) {
        if (opens(this.value, value)) dismiss()
        this.value = value
    }
}

/** A [Cover] remembered with [keys], clearing this composition's focus when it opens. */
@Composable
internal fun <T> rememberCover(vararg keys: Any?, initial: () -> T): Cover<T> {
    val focus = LocalFocusManager.current
    return remember(*keys) { Cover(initial()) { focus.clearFocus(force = true) } }
}
