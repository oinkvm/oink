package com.hachiwii.oink.control

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** docs/control-page.md §3.1: modifier states, Fn layer, temporary keyboard switch, send timing. */
class KeyboardModelTest {
    private var t = 1_000L
    private val sent = ArrayList<Pair<Int, Boolean>>()
    private val typed = ArrayList<String>()
    /** Repeat timers, run by [advance]. */
    private class Timer(val at: Long, val action: () -> Unit) { var live = true }
    private val timers = ArrayList<Timer>()
    private val clock = RepeatClock { ms, run -> val timer = Timer(t + ms, run); timers += timer; { timer.live = false } }
    private fun advance(ms: Long) {
        val end = t + ms
        while (true) {
            val next = timers.filter { it.live && it.at <= end }.minByOrNull { it.at } ?: break
            timers.remove(next); t = next.at; next.action()
        }
        t = end
    }
    /** The core's answer for a character key ([ControlLink.typedChar]); US unless a test sets its own. */
    private var keys: (String, Boolean, Char, Set<Mod>) -> TypedChar? = UsKeys::typed
    private fun model(kind: KeyboardKind = KeyboardKind.SYSTEM) =
        KeyboardModel(kind, { t }, clock, { typed += it }, { layout, mac, ch, mods -> keys(layout, mac, ch, mods) }) { code, down -> sent += code to down }
    private fun d(code: Int) = code to true
    private fun u(code: Int) = code to false

    @Test fun tapHoldsAndAKeyReleasesHeldModifiers() {
        val k = model()
        k.tapModifier(Mod.CTRL); t += 400
        k.tapModifier(Mod.SHIFT); t += 400
        assertEquals(Latch.HELD, k.mods[Mod.CTRL])
        k.pressChar("t", 't'); k.release("t")
        assertEquals(listOf(d(Hid.L_CTRL), d(Hid.L_SHIFT), d(Hid.key(0x17)), u(Hid.key(0x17)), u(Hid.L_SHIFT), u(Hid.L_CTRL)), sent)
        assertEquals(emptyMap<Mod, Latch>(), k.mods.toMap())
    }

    @Test fun longPressLocksAndLockedModifierSurvivesKeys() {
        val k = model()
        k.lockModifier(Mod.ALT)
        assertEquals(Latch.LOCKED, k.mods[Mod.ALT])
        k.click(Hid.TAB)
        assertEquals(listOf(d(Hid.L_ALT), d(Hid.TAB), u(Hid.TAB)), sent)
        t += 1000
        k.tapModifier(Mod.ALT)
        assertNull(k.mods[Mod.ALT])
        assertEquals(u(Hid.L_ALT), sent.last())
    }

    @Test fun longPressOnAHeldModifierLocksItAndOnALockedOneReleasesIt() {
        val k = model()
        k.tapModifier(Mod.CTRL)
        k.lockModifier(Mod.CTRL)
        assertEquals(Latch.LOCKED, k.mods[Mod.CTRL])
        assertEquals(listOf(d(Hid.L_CTRL)), sent) // already down: no second down
        k.lockModifier(Mod.CTRL)
        assertNull(k.mods[Mod.CTRL])
        assertEquals(listOf(d(Hid.L_CTRL), u(Hid.L_CTRL)), sent)
    }

    /** User 2026-11-12: a double tap is the modifier pressed and released once, never a lock. */
    @Test fun doubleTapIsOneShotPressAndRelease() {
        val k = model()
        k.tapModifier(Mod.META); t += 200
        k.tapModifier(Mod.META)
        assertNull(k.mods[Mod.META])
        assertEquals(listOf(d(Hid.L_GUI), u(Hid.L_GUI)), sent)
        // A third quick tap starts over: held.
        t += 100; k.tapModifier(Mod.META)
        assertEquals(Latch.HELD, k.mods[Mod.META])
        sent.clear()
        // From held (slow cancel, then a quick tap): up, then a whole down + up; it stays released.
        t += 1000; k.tapModifier(Mod.META)
        t += 150; k.tapModifier(Mod.META)
        assertNull(k.mods[Mod.META])
        assertEquals(listOf(u(Hid.L_GUI), d(Hid.L_GUI), u(Hid.L_GUI)), sent)
        // From locked: the same.
        sent.clear()
        k.lockModifier(Mod.ALT); sent.clear()
        t += 1000; k.tapModifier(Mod.ALT); t += 100; k.tapModifier(Mod.ALT)
        assertNull(k.mods[Mod.ALT])
        assertEquals(listOf(u(Hid.L_ALT), d(Hid.L_ALT), u(Hid.L_ALT)), sent)
    }

    @Test fun quickTapAfterAKeyUsedTheHeldModifierHoldsItAgain() {
        val k = model()
        k.tapModifier(Mod.CTRL); t += 100
        k.click(Hid.C); t += 100
        k.tapModifier(Mod.CTRL)
        assertEquals(Latch.HELD, k.mods[Mod.CTRL])
        assertEquals(listOf(d(Hid.L_CTRL), d(Hid.C), u(Hid.C), u(Hid.L_CTRL), d(Hid.L_CTRL)), sent)
    }

    @Test fun slowSecondTapCancelsAHeldModifier() {
        val k = model()
        k.tapModifier(Mod.CTRL); t += 301
        k.tapModifier(Mod.CTRL)
        assertNull(k.mods[Mod.CTRL])
        assertEquals(listOf(d(Hid.L_CTRL), u(Hid.L_CTRL)), sent)
    }

    @Test fun modifierOverSystemKeyboardSwitchesTemporarily() {
        val k = model(KeyboardKind.SYSTEM)
        k.tapModifier(Mod.CTRL)
        assertEquals(KeyboardKind.EMULATED, k.shown)
        assertEquals(KeyboardKind.SYSTEM, k.preferred)
        k.click(Hid.key(0x04))
        assertEquals(KeyboardKind.SYSTEM, k.shown)
    }

    @Test fun switchKeyDuringTemporarySwitchReleasesAndGoesBack() {
        val k = model(KeyboardKind.SYSTEM)
        k.lockModifier(Mod.SHIFT)
        k.switchKind()
        assertEquals(KeyboardKind.SYSTEM, k.shown)
        assertEquals(KeyboardKind.SYSTEM, k.preferred)
        assertEquals(u(Hid.L_SHIFT), sent.last())
    }

    @Test fun manualEmulatedStaysAfterModifiersRelease() {
        val k = model(KeyboardKind.SYSTEM)
        k.switchKind()
        assertEquals(KeyboardKind.EMULATED, k.preferred)
        k.tapModifier(Mod.CTRL); k.click(Hid.key(0x04))
        assertEquals(KeyboardKind.EMULATED, k.shown)
    }

    @Test fun autoShiftWrapsOnlyThatKeyAndNeverReleasesTheUsersShift() {
        val k = model()
        k.pressChar("!", '!'); k.release("!")
        assertEquals(listOf(d(Hid.L_SHIFT), d(Hid.key(0x1E)), u(Hid.key(0x1E)), u(Hid.L_SHIFT)), sent)
        sent.clear()
        k.lockModifier(Mod.SHIFT)
        sent.clear()
        k.pressChar("@", '@'); k.release("@")
        assertEquals(listOf(d(Hid.key(0x1F)), u(Hid.key(0x1F))), sent)
        assertEquals(Latch.LOCKED, k.mods[Mod.SHIFT])
    }

    @Test fun ctrlWithAutoShiftSymbol() {
        val k = model()
        k.tapModifier(Mod.CTRL)
        k.pressChar("!", '!'); k.release("!")
        assertEquals(listOf(d(Hid.L_CTRL), d(Hid.L_SHIFT), d(Hid.key(0x1E)), u(Hid.key(0x1E)), u(Hid.L_SHIFT), u(Hid.L_CTRL)), sent)
    }

    @Test fun copyMergesWithHeldModifiersPerPlatform() {
        val k = model()
        k.shortcut(Hid.C, mac = true)
        assertEquals(listOf(d(Hid.L_GUI), d(Hid.C), u(Hid.C), u(Hid.L_GUI)), sent)
        sent.clear()
        k.tapModifier(Mod.SHIFT)
        k.shortcut(Hid.V, mac = false)
        assertEquals(listOf(d(Hid.L_SHIFT), d(Hid.L_CTRL), d(Hid.V), u(Hid.V), u(Hid.L_CTRL), u(Hid.L_SHIFT)), sent)
        sent.clear()
        // Ctrl already down: no second Ctrl down / up around the letter.
        k.lockModifier(Mod.CTRL)
        sent.clear()
        k.shortcut(Hid.C, mac = false)
        assertEquals(listOf(d(Hid.C), u(Hid.C)), sent)
    }

    @Test fun mediaKeysIgnoreModifiers() {
        val k = model()
        k.tapModifier(Mod.CTRL)
        sent.clear()
        k.press("vol", Hid.VOLUME_UP, combines = false); k.release("vol")
        k.media(Hid.PLAY_PAUSE)
        assertEquals(listOf(d(Hid.VOLUME_UP), u(Hid.VOLUME_UP), d(Hid.PLAY_PAUSE), u(Hid.PLAY_PAUSE)), sent)
        assertEquals(Latch.HELD, k.mods[Mod.CTRL])
    }

    @Test fun fnLayerTapDoubleTapAndModifier() {
        val k = model()
        k.tapFn(); assertEquals(Latch.HELD, k.fn)
        k.tapModifier(Mod.CTRL)
        k.fKey(4)
        assertNull(k.fn)
        assertEquals(listOf(d(Hid.L_CTRL), d(Hid.f(4)), u(Hid.f(4)), u(Hid.L_CTRL)), sent)
        t += 1000
        k.tapFn(); t += 100; k.tapFn()
        assertEquals(Latch.LOCKED, k.fn)
        k.fKey(1)
        assertEquals(Latch.LOCKED, k.fn)
        t += 1000; k.tapFn()
        assertNull(k.fn)
    }

    @Test fun closingReleasesEverythingAndResetsFn() {
        val k = model()
        k.lockModifier(Mod.META)
        k.press("up", Hid.UP)
        k.tapFn()
        sent.clear()
        k.closed()
        assertEquals(listOf(u(Hid.UP), u(Hid.L_GUI)), sent)
        assertNull(k.fn)
        assertEquals(0, k.mods.size)
    }

    @Test fun heldKeyReleasesOnlyOnce() {
        val k = model()
        k.press("left", Hid.LEFT); k.release("left"); k.release("left")
        assertEquals(listOf(d(Hid.LEFT), u(Hid.LEFT)), sent)
    }

    // ---- Key repeat (user 2026-11-12) ----

    @Test fun heldKeyRepeatsAfterTheDelayUntilItGoesUp() {
        val k = model()
        k.press("backspace", Hid.BACKSPACE)
        advance(KeyboardModel.REPEAT_DELAY_MS - 1)
        assertEquals(listOf(d(Hid.BACKSPACE)), sent)
        advance(1)                                        // first repeat
        advance(KeyboardModel.REPEAT_INTERVAL_MS * 3)     // three more
        assertEquals(List(5) { d(Hid.BACKSPACE) }, sent)
        k.release("backspace")
        advance(5_000)
        assertEquals(List(5) { d(Hid.BACKSPACE) } + u(Hid.BACKSPACE), sent)
    }

    @Test fun characterRepeatsInsideItsAutoShift() {
        val k = model()
        k.pressChar("vk-!", '!')
        advance(KeyboardModel.REPEAT_DELAY_MS + KeyboardModel.REPEAT_INTERVAL_MS)
        k.release("vk-!")
        assertEquals(listOf(d(Hid.L_SHIFT), d(Hid.key(0x1E)), d(Hid.key(0x1E)), d(Hid.key(0x1E)), u(Hid.key(0x1E)), u(Hid.L_SHIFT)), sent)
    }

    @Test fun onlyTheNewestHeldKeyRepeatsAndAnOlderOneDoesNotStartAgain() {
        val k = model()
        k.pressChar("vk-a", 'a'); advance(KeyboardModel.REPEAT_DELAY_MS)
        k.pressChar("vk-s", 's'); advance(KeyboardModel.REPEAT_DELAY_MS)
        k.release("vk-s"); advance(2_000)
        k.release("vk-a")
        val a = Hid.key(0x04); val s = Hid.key(0x16)
        assertEquals(listOf(d(a), d(a), d(s), d(s), u(s), u(a)), sent)
    }

    @Test fun mediaVolumeKeysRepeatButModifiersAndClicksDoNot() {
        val k = model()
        k.tapModifier(Mod.CTRL)
        k.click(Hid.ESCAPE)
        advance(2_000)
        assertEquals(listOf(d(Hid.L_CTRL), d(Hid.ESCAPE), u(Hid.ESCAPE), u(Hid.L_CTRL)), sent)
        sent.clear()
        k.press("vol+", Hid.VOLUME_UP, combines = false)
        advance(KeyboardModel.REPEAT_DELAY_MS)
        k.release("vol+")
        assertEquals(listOf(d(Hid.VOLUME_UP), d(Hid.VOLUME_UP), u(Hid.VOLUME_UP)), sent)
    }

    @Test fun closingOrSwitchingTheKeyboardStopsTheRepeat() {
        val k = model(KeyboardKind.EMULATED)
        k.press("up", Hid.UP)
        k.closed()
        advance(5_000)
        assertEquals(listOf(d(Hid.UP), u(Hid.UP)), sent)
        sent.clear()
        k.press("down", Hid.DOWN)
        k.switchKind()
        advance(5_000)
        assertEquals(listOf(d(Hid.DOWN), u(Hid.DOWN)), sent)
    }

    // ---- 键盘布局 (user 2026-11-12) ----

    // Which key each layout uses is the core's (Rust oink-mobile-core key_layouts tests); here: what
    // the keyboard asks it and how it sends the answer.

    @Test fun theCoreGetsTheLayoutTheSystemAndTheModifiersDown() {
        val asked = ArrayList<List<Any>>()
        keys = { layout, mac, ch, mods -> asked += listOf(layout, mac, ch, mods); TypedChar.Key(Hid.key(0x1C)) }
        val k = model(KeyboardKind.EMULATED).apply { layout = "de"; targetMac = true }
        k.lockModifier(Mod.CTRL); k.tapModifier(Mod.SHIFT)
        k.pressChar("z", 'z'); k.release("z")
        assertEquals(listOf(listOf("de", true, 'z', setOf(Mod.CTRL, Mod.SHIFT))), asked)
        assertEquals(listOf(d(Hid.L_CTRL), d(Hid.L_SHIFT), d(Hid.key(0x1C)), u(Hid.key(0x1C)), u(Hid.L_SHIFT)), sent)
    }

    @Test fun altGrOrOptionGoesAroundJustThisKeyInsideTheShift() {
        // German Windows @ = AltGr+Q (right Alt) …
        keys = { _, _, _, _ -> TypedChar.Key(Hid.key(0x14), autoAlt = Hid.R_ALT) }
        val k = model(KeyboardKind.EMULATED)
        k.pressChar("@", '@'); k.release("@")
        assertEquals(listOf(d(Hid.R_ALT), d(Hid.key(0x14)), u(Hid.key(0x14)), u(Hid.R_ALT)), sent)
        sent.clear()
        // … German Mac \ = Option+Shift+7.
        keys = { _, _, _, _ -> TypedChar.Key(Hid.key(0x24), autoShift = true, autoAlt = Hid.L_ALT) }
        k.pressChar("\\", '\\'); k.release("\\")
        assertEquals(listOf(d(Hid.L_SHIFT), d(Hid.L_ALT), d(Hid.key(0x24)), u(Hid.key(0x24)), u(Hid.L_ALT), u(Hid.L_SHIFT)), sent)
    }

    @Test fun deadKeyIsFollowedBySpaceAndDoesNotStayDown() {
        keys = { _, _, _, _ -> TypedChar.Key(Hid.key(0x35), dead = true) }
        val k = model(KeyboardKind.EMULATED).apply { layout = "de" }
        k.pressChar("^", '^')
        advance(2_000)
        k.release("^")
        assertEquals(listOf(d(Hid.key(0x35)), u(Hid.key(0x35)), d(Hid.SPACE), u(Hid.SPACE)), sent)
    }

    @Test fun deadKeyTakesItsShiftAndReleasesHeldModifiers() {
        // Spanish Windows ^ = dead Shift + the key right of P.
        keys = { _, _, _, _ -> TypedChar.Key(Hid.key(0x2F), autoShift = true, dead = true) }
        val k = model(KeyboardKind.EMULATED)
        k.tapModifier(Mod.CTRL); sent.clear()
        k.pressChar("^", '^'); k.release("^")
        assertEquals(listOf(d(Hid.L_SHIFT), d(Hid.key(0x2F)), u(Hid.key(0x2F)), u(Hid.L_SHIFT), d(Hid.SPACE), u(Hid.SPACE), u(Hid.L_CTRL)), sent)
    }

    @Test fun characterWithoutAKeyOnTheLayoutGoesAsText() {
        keys = { _, _, ch, _ -> TypedChar.Text(ch.toString()) }
        val k = model(KeyboardKind.EMULATED).apply { layout = "fr"; targetMac = true }
        k.tapModifier(Mod.SHIFT); sent.clear()
        k.pressChar("[", '[')
        advance(2_000)
        k.release("[")
        assertEquals(listOf("["), typed)
        // No key; the held Shift lets go as after any key.
        assertEquals(listOf(u(Hid.L_SHIFT)), sent)
    }

    @Test fun withoutASessionNothingGoesOut() {
        keys = { _, _, _, _ -> null }
        val k = model(KeyboardKind.EMULATED)
        k.pressChar("a", 'a'); advance(2_000); k.release("a")
        assertEquals(emptyList<Pair<Int, Boolean>>(), sent)
        assertEquals(emptyList<String>(), typed)
    }
    @Test fun capsLockUsesRealShortAndLongPressesWithoutTimersOrRepeat() {
        val k = model(KeyboardKind.EMULATED)
        for (duration in listOf(60L, 1_200L, 5_000L)) {
            sent.clear()
            k.capsLock(true)
            advance(duration)
            assertEquals("down until the finger lifts, without repeats", listOf(d(Hid.CAPS_LOCK)), sent)
            k.capsLock(false)
            assertEquals(listOf(d(Hid.CAPS_LOCK), u(Hid.CAPS_LOCK)), sent)
            advance(5_000)
            assertEquals(2, sent.size)
        }
        assertEquals("Caps never schedules a synthetic up or repeat", 0, timers.size)
    }

    @Test fun capsDoesNotReleaseModifiersAndOtherKeysStillRepeat() {
        val k = model(KeyboardKind.EMULATED)
        k.tapModifier(Mod.CTRL); sent.clear()
        k.press("left", Hid.LEFT)
        k.capsLock(true)
        advance(KeyboardModel.REPEAT_DELAY_MS)
        k.capsLock(false)
        assertEquals(Latch.HELD, k.mods[Mod.CTRL])
        k.release("left")
        assertEquals(listOf(d(Hid.LEFT), d(Hid.CAPS_LOCK), d(Hid.LEFT), u(Hid.CAPS_LOCK), u(Hid.LEFT), u(Hid.L_CTRL)), sent)
    }

    @Test fun quickRetapAndDuplicateTouchCallbacksDoNotShortenOrExtendCaps() {
        val k = model(KeyboardKind.EMULATED)
        k.capsLock(true); k.capsLock(true)
        advance(60); k.capsLock(false); k.capsLock(false)
        advance(40); k.capsLock(true)
        advance(1_200)
        assertEquals(listOf(d(Hid.CAPS_LOCK), u(Hid.CAPS_LOCK), d(Hid.CAPS_LOCK)), sent)
        k.capsLock(false)
        assertEquals(u(Hid.CAPS_LOCK), sent.last())
    }

    @Test fun closingOrSwitchingTheKeyboardLetsCapsLockGoAtOnce() {
        val k = model(KeyboardKind.EMULATED)
        k.capsLock(true); k.closed(); k.capsLock(false)
        k.capsLock(true); k.switchKind(); k.capsLock(false)
        advance(5_000)
        assertEquals(listOf(d(Hid.CAPS_LOCK), u(Hid.CAPS_LOCK), d(Hid.CAPS_LOCK), u(Hid.CAPS_LOCK)), sent)
    }

    @Test fun capsSlideOffLiftCancelAndLastFingerRelease() {
        val k = model()
        val caps = SlideKey("caps", hold = true, down = { k.capsLock(true) }, up = { k.capsLock(false) })
        val slider = KeySlider()
        slider.press(1, caps)
        assertEquals(setOf("caps"), slider.down)
        slider.move(1, Hit.Out)
        assertEquals(emptySet<String>(), slider.down)
        slider.move(1, Hit.On(caps)); slider.lift(1)
        assertEquals(listOf(d(Hid.CAPS_LOCK), u(Hid.CAPS_LOCK)), sent)
        slider.press(2, caps); slider.cancel(2)
        slider.press(3, caps); slider.press(4, caps); slider.lift(3)
        assertEquals(d(Hid.CAPS_LOCK), sent.last())
        slider.lift(4)
        assertEquals(List(3) { listOf(d(Hid.CAPS_LOCK), u(Hid.CAPS_LOCK)) }.flatten(), sent)
        assertEquals(0, timers.size)
    }

    @Test fun turningThePageTakesThatOrientationsKind() {
        val k = model(KeyboardKind.SYSTEM)
        k.restore(KeyboardKind.EMULATED)
        assertEquals(KeyboardKind.EMULATED, k.preferred)
        k.lockModifier(Mod.SHIFT); sent.clear()
        // Back to the system keyboard: the modifiers go up, as with the switch key.
        k.restore(KeyboardKind.SYSTEM)
        assertEquals(KeyboardKind.SYSTEM, k.shown)
        assertEquals(listOf(u(Hid.L_SHIFT)), sent)
        // The same kind again changes nothing.
        k.restore(KeyboardKind.SYSTEM)
        assertEquals(1, sent.size)
    }
}
