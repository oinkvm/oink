package com.hachiwii.oink.audio

import android.media.AudioAttributes
import android.media.AudioManager
import android.os.Build
import uniffi.oink_mobile_ffi.MobileExitVolumeEndpoint
import uniffi.oink_mobile_ffi.MobileExitVolumeReading
import uniffi.oink_mobile_ffi.MobileExitVolumeStatus
import uniffi.oink_mobile_ffi.MobileVolumeKey
import kotlin.math.roundToInt

/** The system media stream as the exit volume uses it (a seam for tests). */
internal interface MediaStreamVolume {
    /**
     * A fenced write is possible at all. API 30–32 lack a public
     * attributes-to-output query, and a fixed-volume device has nothing to set.
     */
    val adjustable: Boolean
    /** Id of the single output the media attributes route to now; null if none or several. */
    fun routedOutput(): Int?
    val minIndex: Int
    val maxIndex: Int
    var index: Int
    val muted: Boolean
    /** `AudioManager.adjustStreamVolume` on the media stream. */
    fun adjust(direction: Int, flags: Int)
}

private class AudioManagerStream(
    private val manager: AudioManager,
    private val attributes: AudioAttributes,
) : MediaStreamVolume {
    private val stream = AudioManager.STREAM_MUSIC
    override val adjustable get() = Build.VERSION.SDK_INT >= 33 && !manager.isVolumeFixed
    override fun routedOutput(): Int? =
        if (Build.VERSION.SDK_INT >= 33) manager.getAudioDevicesForAttributes(attributes).singleOrNull()?.id else null
    override val minIndex get() = manager.getStreamMinVolume(stream)
    override val maxIndex get() = manager.getStreamMaxVolume(stream)
    override var index: Int
        get() = manager.getStreamVolume(stream)
        set(value) = manager.setStreamVolume(stream, value, 0)
    override val muted get() = manager.isStreamMute(stream)
    override fun adjust(direction: Int, flags: Int) = manager.adjustStreamVolume(stream, direction, flags)
}

/**
 * Uses the system media stream only while its unique route matches the live AAudio output.
 *
 * While the media stream is muted AudioService's getStreamVolume() returns 0 by convention, and the
 * index it restores on unmute (getLastAudibleStreamVolume) is not public API. Reporting that 0 told
 * the PCs the exit was at 0 %, so their unmute wrote volume 0: unmuted but silent until the user
 * dragged the slider (the long silence after a remote enable on the test phone, 2026-11-12; Android
 * 15 / HyperOS 2). So the index last seen audible is reported while muted ([reportedIndex]), and an
 * unmute that carries back a 0 we reported for an unknown level only unmutes ([unmuteIndex]).
 */
internal class SystemExitVolume(private val volume: MediaStreamVolume) : MobileExitVolumeEndpoint {
    constructor(manager: AudioManager, attributes: AudioAttributes) : this(AudioManagerStream(manager, attributes))

    private var generation = 0uL
    private var deviceId = 0
    /** The media stream's index the last time it was audible (read or written), for this process. */
    private var audibleIndex: Int? = null

    @Synchronized
    fun bind(generation: ULong, deviceId: Int) {
        this.generation = generation
        this.deviceId = deviceId
    }

    @Synchronized
    fun retire() {
        generation = 0uL
        deviceId = 0
    }

    @Synchronized
    override fun readVolume(generation: ULong): MobileExitVolumeReading {
        return try {
            val status = routeStatus(generation)
            if (status != MobileExitVolumeStatus.READY) return MobileExitVolumeReading(status, null, null)
            val minimum = volume.minIndex
            val maximum = volume.maxIndex
            if (maximum <= minimum) {
                MobileExitVolumeReading(MobileExitVolumeStatus.UNSUPPORTED, null, null)
            } else {
                val muted = volume.muted
                val current = volume.index.coerceIn(minimum, maximum)
                if (!muted) audibleIndex = current
                val index = reportedIndex(muted, current, audibleIndex)
                val percent = ((index - minimum) * 100.0 / (maximum - minimum)).roundToInt()
                MobileExitVolumeReading(MobileExitVolumeStatus.READY, percent.toUByte(), muted)
            }
        } catch (_: SecurityException) {
            MobileExitVolumeReading(MobileExitVolumeStatus.PERMISSION_DENIED, null, null)
        } catch (_: RuntimeException) {
            MobileExitVolumeReading(MobileExitVolumeStatus.BACKEND_FAILED, null, null)
        }
    }

    @Synchronized
    override fun writeVolume(generation: ULong, volumePercent: UByte, muted: Boolean): MobileExitVolumeStatus {
        return try {
            val status = routeStatus(generation)
            if (status != MobileExitVolumeStatus.READY) return status
            val minimum = volume.minIndex
            val maximum = volume.maxIndex
            if (maximum <= minimum) return MobileExitVolumeStatus.UNSUPPORTED
            var target = minimum + ((maximum - minimum) * volumePercent.toInt() / 100.0).roundToInt()
            if (volume.muted && !muted) {
                volume.adjust(AudioManager.ADJUST_UNMUTE, 0)
                target = unmuteIndex(target, minimum, audibleIndex, volume.index)
            }
            val current = routeStatus(generation)
            if (current != MobileExitVolumeStatus.READY) return current
            if (volume.index != target) volume.index = target
            val finalRoute = routeStatus(generation)
            if (finalRoute != MobileExitVolumeStatus.READY) return finalRoute
            if (muted && !volume.muted) {
                volume.adjust(AudioManager.ADJUST_MUTE, 0)
            }
            val after = routeStatus(generation)
            if (after != MobileExitVolumeStatus.READY) return after
            val observedMute = volume.muted
            val observedVolume = volume.index
            if ((muted && !observedMute) || (!muted && observedVolume != target)
                || (!muted && target != 0 && observedMute)
            ) MobileExitVolumeStatus.BACKEND_FAILED else {
                audibleIndex = target
                MobileExitVolumeStatus.READY
            }
        } catch (_: SecurityException) {
            MobileExitVolumeStatus.PERMISSION_DENIED
        } catch (_: RuntimeException) {
            MobileExitVolumeStatus.BACKEND_FAILED
        }
    }

    /**
     * A volume key forwarded to this phone as the exit (user 2026-11-12): what a desktop exit does
     * by posting the native media key. The system takes its own step (raising also unmutes) or
     * toggles the mute, and shows its volume panel as the desktop shows its HUD. The core reads
     * the result back with [readVolume] and publishes it in the exit volume state.
     */
    @Synchronized
    override fun keyVolume(generation: ULong, key: MobileVolumeKey): MobileExitVolumeStatus {
        return try {
            val status = routeStatus(generation)
            if (status != MobileExitVolumeStatus.READY) return status
            val direction = when (key) {
                MobileVolumeKey.UP -> AudioManager.ADJUST_RAISE
                MobileVolumeKey.DOWN -> AudioManager.ADJUST_LOWER
                MobileVolumeKey.MUTE -> AudioManager.ADJUST_TOGGLE_MUTE
            }
            volume.adjust(direction, AudioManager.FLAG_SHOW_UI)
            routeStatus(generation)
        } catch (_: SecurityException) {
            MobileExitVolumeStatus.PERMISSION_DENIED
        } catch (_: RuntimeException) {
            MobileExitVolumeStatus.BACKEND_FAILED
        }
    }

    private fun routeStatus(expected: ULong): MobileExitVolumeStatus {
        if (generation == 0uL || generation != expected) return MobileExitVolumeStatus.UNAVAILABLE
        // A fixed or ambiguous route cannot provide a fenced system-volume write.
        if (!volume.adjustable || deviceId <= 0) return MobileExitVolumeStatus.UNSUPPORTED
        return if (volume.routedOutput() == deviceId) MobileExitVolumeStatus.READY else MobileExitVolumeStatus.UNAVAILABLE
    }
}

/** The index to report: the stream's own while audible, else the last audible one this process saw. */
internal fun reportedIndex(muted: Boolean, streamVolume: Int, audibleIndex: Int?): Int =
    if (muted) audibleIndex ?: streamVolume else streamVolume

/**
 * The index to set when unmuting. With no audible index known we reported the muted stream's 0
 * ([reportedIndex]); a request carrying that minimum back is the PC's unmute, not a chosen level, so
 * the index Android restored on unmute ([restored]) stays.
 */
internal fun unmuteIndex(target: Int, minimum: Int, audibleIndex: Int?, restored: Int): Int =
    if (audibleIndex == null && target == minimum) restored else target
