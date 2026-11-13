package com.hachiwii.oink.audio

/** Control operations only. PCM callbacks and codec wakeups stay in native code. */
internal class NativeAudio(private val failed: (Long, Int) -> Unit) {
    companion object { init { System.loadLibrary("oink_android_audio") } }
    external fun open(input: Boolean, generation: Long, exclusive: Boolean): Long
    external fun address(handle: Long): Long
    external fun start(handle: Long): Int
    external fun sessionId(handle: Long): Int
    external fun deviceId(handle: Long): Int
    external fun close(handle: Long)
    @Suppress("unused") // Called by JNI on the non-realtime worker.
    fun onNativeError(generation: Long, error: Int) = failed(generation, error)
}
