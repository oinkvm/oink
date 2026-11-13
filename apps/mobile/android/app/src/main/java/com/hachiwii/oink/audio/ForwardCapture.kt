package com.hachiwii.oink.audio

import android.os.*
import android.system.*
import com.hachiwii.oink.privilege.PrivilegeClient
import com.hachiwii.oink.privilege.PrivilegeWire
import com.hachiwii.oink.privilege.OutputCapture
import java.io.Closeable
import java.io.EOFException
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.atomic.AtomicBoolean
import uniffi.oink_mobile_ffi.*

/** Serial non-realtime pipe reader. The helper captures; Rust alone applies gain/encodes/sends. */
internal class ForwardCapture(
    private val media: MobileMediaSession,
    private val failed: (ULong, MobileMediaFailure) -> Unit,
) : Closeable {
    private val native = NativeForward()
    private var active: Capture? = null

    /** Called on the audio owner's IO worker after claimStart(FORWARD). */
    fun start(generation: ULong): MobileMediaFailure? {
        check(active == null)
        if (!PrivilegeClient.available()) return MobileMediaFailure.SYSTEM_BLOCKED
        val instance = try {
            val health = PrivilegeClient.request(Bundle().apply { putString("op", "health") })
            if (health.getString("status") != "enabled") return MobileMediaFailure.SYSTEM_BLOCKED
            checkNotNull(health.getString("incarnation"))
        } catch (_: Exception) { return MobileMediaFailure.SYSTEM_BLOCKED }
        val handle = native.create(OutputCapture.RATE, OutputCapture.CHANNELS, generation.toLong())
        if (handle == 0L) return MobileMediaFailure.UNAVAILABLE
        if (!media.attachAudio(MobileMediaKind.FORWARD, generation, handle.toULong())) {
            native.revoke(handle); native.destroy(handle)
            return MobileMediaFailure.INTERRUPTED
        }
        val capture = try { Capture(generation, instance, handle) }
        catch (_: Exception) {
            native.revoke(handle); native.destroy(handle)
            return MobileMediaFailure.UNAVAILABLE
        }
        active = capture
        try {
            val response = PrivilegeClient.request(Bundle().apply {
                putString("op", "start_output_capture")
                putLong("generation", generation.toLong())
                putParcelable("pipe", capture.pipe[1])
                putBinder("capture_callback", capture.callback)
            }, instance)
            // Only the helper owns a writer now: helper death must be observed as EOF.
            capture.pipe[1].close()
            val failure = when (response.getString("status")) {
                "ok" -> null
                "capture_busy" -> MobileMediaFailure.UNAVAILABLE
                else -> MobileMediaFailure.SYSTEM_BLOCKED
            }
            if (failure != null) { close(); return failure }
            capture.reader.start()
            return null
        } catch (_: Exception) {
            close()
            return MobileMediaFailure.SYSTEM_BLOCKED
        }
    }

    private inner class Capture(val generation: ULong, val instance: String, val handle: Long) {
        val pipe = ParcelFileDescriptor.createReliablePipe()
        val wake = try { ParcelFileDescriptor.createPipe() }
        catch (e: Exception) { pipe.forEach { runCatching { it.close() } }; throw e }
        val closed = AtomicBoolean()
        val heldKeys = mutableSetOf<UShort>()
        // A separate authenticated Binder avoids replacing the input/clipboard owner's listener.
        val callback = object : Binder() {
            override fun onTransact(code: Int, data: Parcel, reply: Parcel?, flags: Int): Boolean {
                if (code != PrivilegeWire.CALL) return false
                if (Binder.getCallingUid() != 2000) throw SecurityException("shell required")
                data.enforceInterface(PrivilegeWire.DESCRIPTOR)
                val event = data.readBundle(javaClass.classLoader) ?: return true
                if (closed.get() || event.getString("incarnation") != instance ||
                    event.getLong("generation").toULong() != generation) return true
                when (event.getString("status")) {
                    "output_volume_key" -> synchronized(heldKeys) {
                        if (!closed.get()) {
                            val usage = event.getInt("usage").toUShort()
                            val pressed = event.getBoolean("pressed")
                            if (pressed) heldKeys.add(usage) else heldKeys.remove(usage)
                            media.forwardVolumeKey(usage, pressed)
                        }
                    }
                    "output_capture_failed" -> failed(generation, MobileMediaFailure.INTERRUPTED)
                }
                return true
            }
        }
        val reader = Thread({
            try { readFrames() }
            catch (_: Exception) { if (!closed.get()) failed(generation, MobileMediaFailure.INTERRUPTED) }
        }, "OinkForwardPipe")

        private fun readFrames() {
            val fd = pipe[0].fileDescriptor
            Os.fcntlInt(fd, OsConstants.F_SETFL, Os.fcntlInt(fd, OsConstants.F_GETFL, 0) or OsConstants.O_NONBLOCK)
            val audio = StructPollfd().apply { this.fd = fd; events = OsConstants.POLLIN.toShort() }
            val stop = StructPollfd().apply { this.fd = wake[0].fileDescriptor; events = OsConstants.POLLIN.toShort() }
            val bytes = ByteArray(OutputCapture.HEADER_BYTES + OutputCapture.PCM_BYTES)
            val samples = ShortArray(OutputCapture.FRAMES * OutputCapture.CHANNELS)
            var used = 0
            var nextPosition = -1L
            var gap = true
            while (!closed.get()) {
                // A dedicated wake pipe makes stop/join independent of AudioRecord or Binder progress.
                try { Os.poll(arrayOf(audio, stop), -1) }
                catch (e: ErrnoException) { if (e.errno == OsConstants.EINTR) continue else throw e }
                if (closed.get() || stop.revents.toInt() != 0) break
                val n = try { Os.read(fd, bytes, used, bytes.size - used) }
                catch (e: ErrnoException) { if (e.errno == OsConstants.EAGAIN || e.errno == OsConstants.EINTR) continue else throw e }
                if (n == 0) throw EOFException("helper capture ended")
                used += n
                if (used != bytes.size) continue
                used = 0
                val packet = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)
                check(packet.int == OutputCapture.PCM_BYTES)
                val position = packet.long
                val hostNs = packet.long
                val discontinuity = packet.int != 0 || gap || nextPosition != position
                for (i in samples.indices) samples[i] = packet.short
                val accepted = native.push(handle, samples, OutputCapture.FRAMES, hostNs, discontinuity)
                nextPosition = position + OutputCapture.FRAMES
                gap = accepted != OutputCapture.FRAMES
            }
        }
    }
    /** Called on IO before acknowledging the core stop barrier. */
    override fun close() {
        val capture = active ?: return
        active = null
        capture.closed.set(true)
        synchronized(capture.heldKeys) {
            capture.heldKeys.forEach { media.forwardVolumeKey(it, false) }
            capture.heldKeys.clear()
        }
        try { Os.write(capture.wake[1].fileDescriptor, byteArrayOf(1), 0, 1) } catch (_: Exception) { }
        try {
            PrivilegeClient.request(Bundle().apply {
                putString("op", "stop_output_capture")
                putLong("generation", capture.generation.toLong())
            }, capture.instance)
        } catch (_: Exception) { /* The helper's death closes its policy and the writer. */ }
        // Never free the FFI handle until the only PCM producer has left it.
        var interrupted = false
        while (true) try { capture.reader.join(); break } catch (_: InterruptedException) { interrupted = true }
        native.revoke(capture.handle)
        native.destroy(capture.handle)
        (capture.pipe + capture.wake).forEach { runCatching { it.close() } }
        if (interrupted) Thread.currentThread().interrupt()
    }
}

internal class NativeForward {
    companion object { init { System.loadLibrary("oink_android_audio") } }
    external fun create(rate: Int, channels: Int, generation: Long): Long
    external fun push(handle: Long, samples: ShortArray, frames: Int, hostNs: Long, discontinuity: Boolean): Int
    external fun revoke(handle: Long)
    external fun destroy(handle: Long)
}
