package com.hachiwii.oink.recovery

import android.content.Context
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.transformWhile
import kotlinx.coroutines.flow.collect
import uniffi.oink_mobile_ffi.MobileRecovery
import uniffi.oink_mobile_ffi.RecoveryEvent
import uniffi.oink_mobile_ffi.RecoveryRequest
import java.security.MessageDigest
import java.io.File
import java.nio.ByteBuffer
import java.util.UUID

/** Runs only alongside the unlocked, unique Rust group owner. No CE access from boot service. */
class RecoveryOwner(private val context: Context, private val native: MobileRecovery) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO + CoroutineExceptionHandler { _, _ ->
        RecoveryController.report(RecoveryPhase.Failed, RecoveryReason.Unavailable, "recovery_control_failed")
    })
    private val pcResults = Channel<Pair<UByte, UByte>>(Channel.CONFLATED)
    private val attemptedPeers = mutableSetOf<String>()
    @Volatile private var fallback: Job? = null
    private val version by lazy { context.packageManager.getPackageInfo(context.packageName, 0).versionName ?: "" }
    private val digest by lazy {
        val hash = MessageDigest.getInstance("SHA-256")
        File(context.applicationInfo.sourceDir).inputStream().use { input ->
            val buffer = ByteArray(65536)
            while (true) { val count = input.read(buffer); if (count < 0) break; hash.update(buffer, 0, count) }
        }
        hash.digest()
    }
    /**
     * PC-assisted recovery (API 30-32 fallback, PC-started bootstrap) is not in 0.4.0: the core reports
     * `pc_assist_supported() == false` and has no wire messages for it. Then nothing is published to
     * PCs and no PC is asked; only this phone's own wireless-debugging recovery runs.
     */
    private val pcAssist = native.pcAssistSupported()
    init {
        val store = RecoveryStore(context)
        store.bindGroup(native.groupId(), native.localDevice()) { native.members() }
        if (pcAssist) {
            scope.launch { RecoveryController.credentialChanges.collect { publishKeys() } }
            scope.launch { RecoveryController.endpoints.collect { publishEndpoints() } }
        }
        scope.launch {
            if (pcAssist) publishKeys()
            RecoveryController.start(context)
        }
        if (pcAssist) scope.launch {
            while (isActive) {
                try {
                    val event = native.next() ?: break
                    when (event) {
                        is RecoveryEvent.Credential -> {
                            val batch = event.value
                            val before = store.snapshot()
                            var changed = false
                            val accepted = runCatching { changed = store.merge(batch) { native.members() } }.isSuccess
                            val snapshot = store.snapshot()
                            native.credentialAck(batch, accepted, snapshot.epochCounter, snapshot.epochOrigin)
                            if (accepted && changed) {
                                val reset = snapshot.epochCounter != before.epochCounter ||
                                    (snapshot.epochCounter != 0uL && snapshot.epochOrigin != before.epochOrigin)
                                if (reset) RecoveryController.setEnabled(context, false)
                                publishKeys()
                                if (!reset) RecoveryController.start(context)
                            }
                        }
                        is RecoveryEvent.Start -> scope.launch { execute(event.value) }
                        is RecoveryEvent.Result -> pcResults.send(event.phase to event.reason)
                    }
                } catch (error: CancellationException) { throw error }
                catch (_: Exception) {
                    RecoveryController.report(RecoveryPhase.Failed, RecoveryReason.Unavailable, "recovery_control_failed")
                }
            }
        }
        scope.launch {
            RecoveryController.status.collect { status ->
                if (status.phase == RecoveryPhase.Ready) {
                    fallback?.cancel()
                    synchronized(attemptedPeers) { attemptedPeers.clear() }
                } else if (status.phase == RecoveryPhase.Pending) {
                    synchronized(attemptedPeers) { attemptedPeers.clear() }
                } else if (status.phase == RecoveryPhase.Failed) retryFallback()
            }
        }
    }
    private suspend fun execute(request: RecoveryRequest) {
        if (!RecoveryController.enabled(context)) { native.progress(request.request, 4u, 2u); return }
        if (request.helperVersion != version || !request.helperDigest.contentEquals(digest)) {
            native.progress(request.request, 4u, 4u); return
        }
        val remaining = native.remainingMs(request.request).toLong()
        if (remaining == 0L) { native.progress(request.request, 5u, 7u); return }
        RecoveryController.start(context)
        try {
            withTimeout(remaining) {
                RecoveryController.status.transformWhile { status ->
                    emit(status)
                    status.phase !in setOf(RecoveryPhase.Ready, RecoveryPhase.Failed, RecoveryPhase.Expired)
                }.collect { status ->
                    val reason = when (status.reason) {
                        RecoveryReason.None -> 0u
                        RecoveryReason.Unavailable -> 1u
                        RecoveryReason.PermissionDenied -> 2u
                        RecoveryReason.ConnectionFailed -> 5u
                        RecoveryReason.StartFailed -> 6u
                        RecoveryReason.Expired -> 7u
                    }
                    native.progress(request.request, status.phase.ordinal.toUByte(), reason.toUByte())
                }
            }
        } catch (_: TimeoutCancellationException) {
            native.progress(request.request, 5u, 7u)
        }
    }
    private fun publishEndpoints() {
        val snapshot = RecoveryController.endpoints.value
        native.publishEndpoints(snapshot.generation, snapshot.endpoints.map {
            uniffi.oink_mobile_ffi.RecoveryEndpoint(it.address, it.port.toUShort(), it.scopeId.toUInt())
        })
    }
    private fun publishKeys() {
        val store = RecoveryStore(context)
        store.bindGroup(native.groupId(), native.localDevice()) { native.members() }
        val snapshot = store.snapshot()
        val batches = snapshot.records.chunked(16).ifEmpty { listOf(emptyList()) }
        batches.forEach { batch ->
            native.publishCredentials(snapshot.copy(updateId = RecoveryStore.randomId(), records = batch))
        }
    }
    /** Called on group/card changes too, so a newly reachable PC can pick up a failed local start. */
    @Synchronized fun retryFallback() {
        if (!pcAssist) return
        scope.launch { publishKeys(); publishEndpoints() }
        if (!RecoveryController.enabled(context) || RecoveryController.status.value.phase != RecoveryPhase.Failed ||
            RecoveryController.status.value.detail == "helper_crash_limit" || fallback?.isActive == true) return
        fallback = scope.launch {
            for (peer in native.candidates()) {
                if (!synchronized(attemptedPeers) { attemptedPeers.add(peer) }) continue
                val uuid = UUID.randomUUID()
                val id = ByteBuffer.allocate(16).putLong(uuid.mostSignificantBits).putLong(uuid.leastSignificantBits).array()
                while (pcResults.tryReceive().isSuccess) { /* discard previous executor result */ }
                try {
                    native.requestPc(peer, RecoveryRequest(id, version, digest, 30_000u))
                    withTimeoutOrNull(30_000) {
                        while (true) {
                            val result = pcResults.receive()
                            if (result.first >= 3u) break
                        }
                    }
                } catch (e: CancellationException) { throw e }
                catch (_: Exception) { /* try the next reachable group PC */ }
                if (RecoveryController.status.value.phase == RecoveryPhase.Ready) break
            }
        }
    }
    suspend fun close() {
        scope.coroutineContext[Job]?.cancelAndJoin()
        native.destroy()
    }
}
