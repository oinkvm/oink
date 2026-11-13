package com.hachiwii.oink.audio

import android.media.AudioManager
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import uniffi.oink_mobile_ffi.MobileExitVolumeReading
import uniffi.oink_mobile_ffi.MobileExitVolumeStatus
import uniffi.oink_mobile_ffi.MobileVolumeKey

/** A media stream with AudioManager's index/mute semantics for the exit volume. */
private class FakeMediaStream(
    override var adjustable: Boolean = true,
    var output: Int? = 7,
    override val minIndex: Int = 0,
    override val maxIndex: Int = 15,
    override var index: Int = 6,
    override var muted: Boolean = false,
) : MediaStreamVolume {
    val adjusted = mutableListOf<Pair<Int, Int>>()
    override fun routedOutput(): Int? = output
    override fun adjust(direction: Int, flags: Int) {
        adjusted += direction to flags
        when (direction) {
            AudioManager.ADJUST_RAISE -> { index = (index + 1).coerceAtMost(maxIndex); muted = false }
            AudioManager.ADJUST_LOWER -> { index = (index - 1).coerceAtLeast(minIndex); muted = false }
            AudioManager.ADJUST_TOGGLE_MUTE -> muted = !muted
            AudioManager.ADJUST_MUTE -> muted = true
            AudioManager.ADJUST_UNMUTE -> muted = false
        }
    }
}

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class SystemExitVolumeTest {
    private val stream = FakeMediaStream()
    private val exit = SystemExitVolume(stream).apply { bind(3uL, 7) }

    private fun reading() = exit.readVolume(3uL)

    @Test
    fun forwardedKeysTakeTheSystemStepAndAreReadBack() {
        assertEquals(MobileExitVolumeReading(MobileExitVolumeStatus.READY, 40u, false), reading())
        assertEquals(MobileExitVolumeStatus.READY, exit.keyVolume(3uL, MobileVolumeKey.UP))
        assertEquals(7, stream.index)
        assertEquals(MobileExitVolumeReading(MobileExitVolumeStatus.READY, 47u, false), reading())
        assertEquals(MobileExitVolumeStatus.READY, exit.keyVolume(3uL, MobileVolumeKey.MUTE))
        assertEquals(MobileExitVolumeReading(MobileExitVolumeStatus.READY, 47u, true), reading())
        assertEquals(MobileExitVolumeStatus.READY, exit.keyVolume(3uL, MobileVolumeKey.DOWN))
        assertEquals(MobileExitVolumeReading(MobileExitVolumeStatus.READY, 40u, false), reading())
        // The system's own step, mute toggle and volume panel, as a desktop exit's native key.
        assertEquals(
            listOf(
                AudioManager.ADJUST_RAISE to AudioManager.FLAG_SHOW_UI,
                AudioManager.ADJUST_TOGGLE_MUTE to AudioManager.FLAG_SHOW_UI,
                AudioManager.ADJUST_LOWER to AudioManager.FLAG_SHOW_UI,
            ),
            stream.adjusted,
        )
    }

    @Test
    fun aKeyForAnotherOutputOrGenerationChangesNothing() {
        assertEquals(MobileExitVolumeStatus.UNAVAILABLE, exit.keyVolume(2uL, MobileVolumeKey.UP))
        stream.output = 8
        assertEquals(MobileExitVolumeStatus.UNAVAILABLE, exit.keyVolume(3uL, MobileVolumeKey.UP))
        stream.output = 7
        stream.adjustable = false
        assertEquals(MobileExitVolumeStatus.UNSUPPORTED, exit.keyVolume(3uL, MobileVolumeKey.MUTE))
        exit.retire()
        stream.adjustable = true
        assertEquals(MobileExitVolumeStatus.UNAVAILABLE, exit.keyVolume(3uL, MobileVolumeKey.DOWN))
        assertTrue(stream.adjusted.isEmpty())
        assertEquals(6, stream.index)
    }

    @Test
    fun sliderWritesStillSetTheExactIndex() {
        assertEquals(MobileExitVolumeStatus.READY, exit.writeVolume(3uL, 80u, false))
        assertEquals(12, stream.index)
        assertEquals(MobileExitVolumeStatus.READY, exit.writeVolume(3uL, 80u, true))
        assertEquals(MobileExitVolumeReading(MobileExitVolumeStatus.READY, 80u, true), reading())
    }
}
