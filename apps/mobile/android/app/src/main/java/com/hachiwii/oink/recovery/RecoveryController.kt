package com.hachiwii.oink.recovery

import android.content.Context
import android.content.Intent
import com.hachiwii.oink.privilege.PrivilegeClient
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow

/** These phases map to frozen BootstrapPhase/BootstrapReason in the Rust owner. */
enum class RecoveryPhase { Pending, Connecting, Starting, Ready, Failed, Expired }
enum class RecoveryReason { None, Unavailable, PermissionDenied, ConnectionFailed, StartFailed, Expired }
data class RecoveryStatus(val phase: RecoveryPhase, val reason: RecoveryReason, val detail: String)

data class LocalAdbEndpoint(val address: String, val port: Int, val scopeId: Int)
data class AdbEndpointSnapshot(val generation: ULong, val endpoints: List<LocalAdbEndpoint>)

object RecoveryController {
    private val mutableEndpoints = MutableStateFlow(AdbEndpointSnapshot(0u, emptyList()))
    internal val endpoints = mutableEndpoints.asStateFlow()
    @Synchronized internal fun updateEndpoints(values: List<LocalAdbEndpoint>) {
        mutableEndpoints.value = AdbEndpointSnapshot(mutableEndpoints.value.generation + 1u, values.take(16))
    }
    @Volatile private var application: Context? = null
    private val consecutiveDeaths = java.util.concurrent.atomic.AtomicInteger()
    fun initialize(context: Context) { application = context.applicationContext }

    private val mutableStatus = MutableStateFlow(RecoveryStatus(RecoveryPhase.Pending, RecoveryReason.None, "idle"))
    val status = mutableStatus.asStateFlow()
    internal fun report(phase: RecoveryPhase, reason: RecoveryReason = RecoveryReason.None, detail: String) {
        mutableStatus.value = RecoveryStatus(phase, reason, detail)
    }
    private val mutablePointerProblem = MutableStateFlow<String?>(null)
    /**
     * Why PCs cannot move this phone's pointer while the helper is attached (its "mouse_error"
     * health field, e.g. the companion association could not be created); null when they can.
     */
    val pointerProblem = mutablePointerProblem.asStateFlow()
    internal fun reportPointer(problem: String?) { mutablePointerProblem.value = problem }
    fun enabled(context: Context) = RecoveryStore(context).enabled
    /** UI calls only after explaining the three Shizuku settings and receiving user intent. */
    @Synchronized fun setEnabled(context: Context, enabled: Boolean) {
        RecoveryStore(context).enabled = enabled
        applyEnabled(context, enabled)
    }
    /** Provider applies a previously signed PC disable only if the user intent is still identical. */
    @Synchronized fun compareAndDisable(context: Context, expectedRevision: String): Boolean {
        if (!RecoveryStore(context).compareAndDisable(expectedRevision)) return false
        applyEnabled(context, false)
        return true
    }
    private fun applyEnabled(context: Context, enabled: Boolean) {
        consecutiveDeaths.set(0)
        if (enabled) start(context) else {
            context.stopService(Intent(context, RecoveryService::class.java))
            Thread {
                synchronized(RecoveryController) {
                    // A newer explicit enable wins over a queued stop from an earlier disable.
                    if (!enabled(context) && PrivilegeClient.available()) runCatching {
                        PrivilegeClient.request(android.os.Bundle().apply { putString("op", "stop") })
                    }
                }
            }.start()
            report(RecoveryPhase.Pending, detail = "disabled")
        }
    }
    internal val credentialChanges = kotlinx.coroutines.flow.MutableSharedFlow<Unit>(extraBufferCapacity = 1)
    fun resetCredentials(context: Context) {
        setEnabled(context, false)
        RecoveryStore(context).reset()
        credentialChanges.tryEmit(Unit)
    }
    suspend fun pair(context: Context, host: String, port: Int, code: String): Boolean =
        kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
            val pem = RecoveryStore(context).pairingCredential()
            credentialChanges.tryEmit(Unit)
            try { LocalAdb.pair(java.net.InetAddress.getByName(host), port, code, pem) }
            finally { pem.fill(0) }
            // 「配对并开启」 (or the code sent from the pairing notification) is the user's intent to turn
            // privileged mode on; start() alone does nothing while it is still off (first setup).
            setEnabled(context, true)
            true
        }
    fun forgetGroup(context: Context) {
        setEnabled(context, false)
        RecoveryStore(context).clear()
    }
    /** Explicit foreground App start supports API30–32 without changing system settings. */
    fun start(context: Context, automatic: Boolean = false) {
        if (!enabled(context)) return
        if (PrivilegeClient.available()) { helperChanged(); return }
        report(RecoveryPhase.Pending, detail = "waiting_for_network")
        try { context.startForegroundService(Intent(context, RecoveryService::class.java).putExtra("automatic", automatic)) }
        catch (_: RuntimeException) { report(RecoveryPhase.Failed, RecoveryReason.PermissionDenied, "foreground_service_blocked") }
    }
    /** The single unlocked Rust owner calls this only after helper health publication succeeds. */
    fun ownerReady() {
        val context = application ?: return
        if (enabled(context) && PrivilegeClient.available() &&
            context.getSystemService(android.os.UserManager::class.java).isUserUnlocked) {
            report(RecoveryPhase.Ready, detail = "owner_ready")
        }
    }
    fun ownerUnavailable() {
        val context = application ?: return
        if (!enabled(context)) report(RecoveryPhase.Pending, detail = "disabled")
        else if (status.value.phase == RecoveryPhase.Ready || status.value.detail == "helper_attached") {
            report(RecoveryPhase.Failed, RecoveryReason.Unavailable, "owner_unavailable")
        }
    }
    /** Called after authenticated attach and after Binder death, independently of the input owner. */
    @JvmStatic fun helperChanged() {
        if (application?.let { !enabled(it) } == true) {
            report(RecoveryPhase.Pending, detail = "disabled")
            return
        }
        if (PrivilegeClient.available()) {
            if (status.value.phase != RecoveryPhase.Ready) report(RecoveryPhase.Starting, detail = "helper_attached")
        } else {
            val retry = consecutiveDeaths.incrementAndGet() <= 3
            report(RecoveryPhase.Failed, RecoveryReason.StartFailed, if (retry) "helper_lost" else "helper_crash_limit")
            // Permit at most three automatic restarts per process; PC fallback shares this limit.
            if (retry) application?.let { context ->
                android.os.Handler(android.os.Looper.getMainLooper()).post { start(context, automatic = true) }
            }
        }
    }
}
