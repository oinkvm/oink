package com.hachiwii.oink.clipboard

import android.os.Bundle
import com.hachiwii.oink.ReceivedContent
import com.hachiwii.oink.privilege.PrivilegeClient
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import uniffi.oink_mobile_ffi.MobileClipboard
import java.nio.ByteBuffer
import java.util.UUID

enum class NativeClipboardWrite { Unavailable, Written, Stale }

/**
 * The helper's answer to `clipboard_write`; null for "written" (read back before it counts).
 * A locked or inactive helper only means the shell path is closed right now: the App writes the
 * clip itself, as in ordinary mode (review M42). Any other answer means the clipboard moved on.
 */
internal fun nativeWriteResult(status: String?): NativeClipboardWrite? = when (status) {
    "written" -> null
    "locked", "owner_inactive" -> NativeClipboardWrite.Unavailable
    else -> NativeClipboardWrite.Stale
}

/** Clipboard Binder work has its own serial lane, independent of mouse feedback.
 * No clipboard content is logged or retained outside the existing content owner. */
class PrivilegedClipboardAdapter(
    private val clipboard: MobileClipboard,
    private val nativeChanged: () -> Unit,
    private val validate: (ReceivedContent) -> Boolean,
    private val readable: (Boolean, Boolean) -> Unit,
) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val queue = Channel<suspend () -> Unit>(Channel.UNLIMITED)
    private val pendingWrites = java.util.concurrent.ConcurrentHashMap.newKeySet<CompletableDeferred<NativeClipboardWrite>>()
    @Volatile private var incarnation: String? = null
    private var token = ByteArray(16)
    private var revision = 0L
    private var writeReady = false
    private var broadcastEnabled = false
    init {
        scope.launch {
            for (command in queue) try { command() }
            catch (e: CancellationException) { throw e }
            catch (_: Exception) {
                readable(false, false)
                clipboard.captureNativeText(token, revision.toULong(), null, false, false, null, null)
            }
        }
    }
    fun attach(instance: String) {
        clipboard.setNativeRealtime(true)
        queue.trySend {
            incarnation = instance
            writeReady = false
            val id = UUID.fromString(instance)
            token = ByteBuffer.allocate(16).putLong(id.mostSignificantBits).putLong(id.leastSignificantBits).array()
            revision = 0
            observe(false)
        }
    }
    fun setBroadcastEnabled(enabled: Boolean) {
        queue.trySend {
            broadcastEnabled = enabled
            clipboard.setBroadcastEnabled(enabled)
            observe(false)
        }
    }
    fun changed(fresh: Boolean, expectedIncarnation: String? = null) {
        if (!fresh) {
            queue.trySend {
                if (expectedIncarnation == null || expectedIncarnation == incarnation) observe(false)
            }
            return
        }
        // The revision fence moves at once (it revokes automatic writes in flight); the read waits for
        // the announcement debounce.
        nativeChanged()
        debounced(expectedIncarnation)
    }
    /**
     * Reads a changed clipboard after the PCs' announcement debounce, owned by the core
     * (`native_change_seen` / `native_capture_due`, oink_foundation::clipboard_burst): 250 ms after the last
     * change of a burst, at most 1 s after its first. Each change schedules a check at the burst's current
     * deadline; only the check that finds the deadline passed reads, so a clipboard an app writes several
     * times in a row is read and announced once, in its last state, and the states before it are never read.
     */
    private fun debounced(expectedIncarnation: String?) {
        val wait = clipboard.nativeChangeSeen().toLong()
        scope.launch {
            delay(wait)
            queue.trySend {
                if ((expectedIncarnation == null || expectedIncarnation == incarnation) && clipboard.nativeCaptureDue()) observe(true)
            }
        }
    }
    /**
     * The 粘贴 tile (user 2026-11-11): the helper's `clipboard_read` answer, read now on the caller's
     * thread (not the main thread: it is a Binder call to the helper). It is the explicit paste's own read: it never goes through [observe] / `captureNativeText`,
     * so 「自动读取」 ([broadcastEnabled], and the core's broadcast gate behind it) does not decide whether
     * a manual paste is sent, and the core's capture state is left as it was. Null while the helper path
     * is closed (not attached, helper gone, or the call failed).
     */
    fun readForPaste(): Bundle? {
        val instance = incarnation ?: return null
        if (!PrivilegeClient.available()) return null
        return try { PrivilegeClient.request(Bundle().apply { putString("op", "clipboard_read") }, instance) }
        catch (e: Exception) { null }
    }
    fun detach() {
        clipboard.setNativeRealtime(false)
        nativeChanged()
        queue.trySend {
            incarnation = null
            readable(false, false)
            clipboard.captureNativeText(token, revision.toULong(), null, false, false, null, null)
        }
    }
    private fun request(op: String, body: Bundle.() -> Unit = {}): Bundle {
        val instance = checkNotNull(incarnation)
        return PrivilegeClient.request(Bundle().apply { putString("op", op); body() }, instance)
    }
    private suspend fun observe(fresh: Boolean) {
        if (incarnation == null) return
        val value = request("clipboard_read")
        if (value.containsKey("revision")) revision = value.getLong("revision")
        val text = if (value.getString("status") == "text") value.getString("text") else null
        readable(text != null, writeReady)
        // The clip's HTML goes with its text (user 2026-11-12: every format is sent); the core fingerprints both.
        clipboard.captureNativeText(token, revision.toULong(), text, fresh && broadcastEnabled, value.getBoolean("sensitive"),
            value.getByteArray("marker"), value.getString("html").takeIf { text != null })
    }
    suspend fun write(value: ReceivedContent): NativeClipboardWrite {
        if (value.items.isNotEmpty() || value.text == null) return NativeClipboardWrite.Unavailable
        // HTML above the helper's Binder bound: the App writes the clip itself, as in ordinary mode.
        if ((value.html?.toByteArray(Charsets.UTF_8)?.size ?: 0) > com.hachiwii.oink.privilege.PrivilegeWire.MAX_HTML_BYTES)
            return NativeClipboardWrite.Unavailable
        val completed = CompletableDeferred<NativeClipboardWrite>()
        pendingWrites.add(completed)
        if (!queue.trySend {
            try {
                if (!completed.isActive) return@trySend
                if (incarnation == null || !PrivilegeClient.available()) {
                    completed.complete(NativeClipboardWrite.Unavailable)
                } else if (!validate(value)) {
                    completed.complete(NativeClipboardWrite.Stale)
                } else {
                    // Use the revision last observed by this lane. Reading a newer
                    // revision immediately before write would erase the receive fence.
                    val result = request("clipboard_write") {
                        putString("text", value.text); putLong("revision", revision)
                        putBoolean("sensitive", value.sensitive)
                        value.marker?.let { putByteArray("marker", it) }
                        // Rich text with its plain text as one HTML clip (user 2026-11-12).
                        value.html?.let { putString("html", it) }
                    }
                    val written = nativeWriteResult(result.getString("status"))
                    if (written != null) {
                        // Someone else's change: read after the debounce like any change.
                        if (written == NativeClipboardWrite.Stale) debounced(null)
                        completed.complete(written)
                    } else {
                        val readback = request("clipboard_read")
                        if (readback.getString("status") == "text" && readback.getString("text") == value.text) {
                            revision = readback.getLong("revision")
                            writeReady = true
                            readable(true, true)
                            // What the clipboard holds now, HTML included, is Oink's own write: never announced.
                            clipboard.nativeTextWritten(token, revision.toULong(), value.text, value.sensitive, value.marker,
                                readback.getString("html"))
                            completed.complete(NativeClipboardWrite.Written)
                        } else {
                            debounced(null)
                            completed.complete(NativeClipboardWrite.Stale)
                        }
                    }
                }
            } catch (e: Exception) { completed.completeExceptionally(e) }
        }.isSuccess) { pendingWrites.remove(completed); return NativeClipboardWrite.Stale }
        return try { completed.await() } finally { pendingWrites.remove(completed); completed.cancel() }
    }
    suspend fun close() {
        clipboard.setNativeRealtime(false)
        pendingWrites.forEach { it.complete(NativeClipboardWrite.Stale) }
        queue.close()
        scope.coroutineContext[Job]?.cancelAndJoin()
    }
}
