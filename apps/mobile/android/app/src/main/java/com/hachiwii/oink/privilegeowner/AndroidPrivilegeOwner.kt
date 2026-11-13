package com.hachiwii.oink.privilegeowner

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Bundle
import android.os.UserManager
import androidx.core.content.ContextCompat
import com.hachiwii.oink.input.PrivilegedInputAdapter
import com.hachiwii.oink.privilege.PrivilegeClient
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import uniffi.oink_mobile_ffi.*
import java.nio.ByteBuffer
import java.util.UUID

data class AndroidPrivilegeState(val phase: String = "unavailable", val reason: String = "", val inputReady: Boolean = false, val captureReady: Boolean = false)

/** The existing Rust session stays the sole network/media owner. This adapter
 * serializes Binder events and PC media activations without reading CE before unlock.
 *
 * A PC's request to open this phone's speaker exit or microphone starts here, in the
 * background, with no window, in privileged mode or not and also while the phone is locked
 * (user 2026-11-12); it does not use the helper. Android 14+ only lets a background app add
 * the mediaPlayback / microphone foreground types to a service that is allowed to start
 * from the background and to use while-in-use permissions. The connection service always
 * starts while an Oink screen is visible (TOP), and Android keeps both of those allowances
 * on the service record until it leaves the foreground: a later startForeground() only
 * recomputes allowances that are still denied (ActiveServices.setFgsRestrictionLocked,
 * Android 15; `dumpsys activity services` shows mAllowWiu/mAllowStart_noBinding=PROC_STATE_TOP
 * 1.5 hours after the start on the MIUI test phone). So the types are added from the background
 * exactly as the Quick Settings tiles already do; the former helper-launched Activity was
 * only there to make the App TOP and is gone. Without the persistent notification the
 * service runs only while Oink is on screen, which is TOP itself. */
class AndroidPrivilegeOwner(
    private val context: Context,
    private val session: MobileSession,
    private val media: MobileMediaSession,
    private val clipboard: com.hachiwii.oink.clipboard.PrivilegedClipboardAdapter,
    private val supportedResources: Int,
    private val startResources: suspend (AndroidActivationRequest, () -> Boolean) -> Unit,
    private val inputReadyChanged: (Boolean) -> Unit = {},
) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val queue = Channel<suspend () -> Unit>(Channel.UNLIMITED)
    private val mutableState = MutableStateFlow(AndroidPrivilegeState())
    val state = mutableState.asStateFlow()
    private var controls: AndroidControls? = null
    private var subscription: MobileMediaSubscription? = null
    private var incarnation = ByteArray(16)
    private var helperInstance: String? = null
    private var routedInput: MobileAndroidInput? = null
    private var displayGeneration = 0uL
    private var clipboardRead = false
    private var clipboardWrite = false
    /** Accepted (claimed) PC activations, by [key], until they settle. */
    private val requests = mutableMapOf<String, AndroidActivationRequest>()
    /** Committed to Rust; followed by [reconcile] until their resources run or fail. */
    private val started = mutableSetOf<String>()
    private val reportedReady = mutableSetOf<String>()
    private val unlock = object : android.content.BroadcastReceiver() {
        override fun onReceive(context: Context, intent: android.content.Intent) { refreshHelper() }
    }
    private val input = PrivilegedInputAdapter(
        { failure, instance -> enqueue { if (instance == helperInstance) unavailable(failure.javaClass.simpleName) } },
        { ready -> enqueue { setInputReady(ready) } },
    )

    private fun setInputReady(ready: Boolean) {
        val next = ready && state.value.phase == "ready"
        if (state.value.inputReady == next) return
        mutableState.value = state.value.copy(inputReady = next)
        inputReadyChanged(next)
    }

    private fun enqueue(block: suspend () -> Unit) { queue.trySend(block) }
    private fun key(owner: MobileRemoteMediaOwner) = owner.caller + ":" + owner.bootNonce.joinToString("") { "%02x".format(it) } + ":" + owner.request.joinToString("") { "%02x".format(it) }
    suspend fun start() {
        androidx.core.content.ContextCompat.registerReceiver(context, unlock,
            android.content.IntentFilter(android.content.Intent.ACTION_USER_UNLOCKED), androidx.core.content.ContextCompat.RECEIVER_NOT_EXPORTED)
        scope.launch {
            for (block in queue) try { block() }
            catch (e: CancellationException) { throw e }
            catch (e: Exception) { unavailable(e.javaClass.simpleName) }
        }
        val inputOwner = CompletableDeferred<MobileAndroidInput>()
        routedInput = session.androidInput(object : AndroidInputListener {
            override fun onInput(command: AndroidInputCommand) {
                input.apply(command) { success ->
                    inputOwner.await().acknowledge(command.sequence, success)
                }
            }
        })
        inputOwner.complete(requireNotNull(routedInput))
        controls = session.androidControls(object : AndroidControlListener {
            override fun activate(request: AndroidActivationRequest) = enqueue { this@AndroidPrivilegeOwner.activate(request) }
            override fun release(owner: MobileRemoteMediaOwner) = enqueue {
                val requestKey = key(owner)
                if (requestKey !in started) requests.remove(requestKey)
                reconcile(media.snapshot())
            }
        })
        // PCs may open these in any mode (user 2026-11-12), not only while the helper runs.
        controls?.setDirectResources(supportedResources.toUByte())
        subscription = media.subscribe(object : MobileMediaListener {
            override fun onMediaState(state: MobileMediaState) = enqueue { reconcile(state) }
        })
        PrivilegeClient.setListener { event ->
            // Advance the native receive fence before queuing any work. Media
            // activation may be opening its resources; it must not hide a copy
            // from a concurrent main-thread file/URI clipboard publication.
            when (event.getString("status")) {
                "clipboard_changed" -> clipboard.changed(true, event.getString("incarnation"))
                "keyboard_device_ready", "keyboard_removed" -> input.event(event)
                "ipc_lost" -> {
                    // Invalidate the exact helper before queued activation work can commit.
                    val lost = runCatching { UUID.fromString(event.getString("incarnation")) }.getOrNull()
                    if (lost != null) {
                        val bytes = ByteBuffer.allocate(16).putLong(lost.mostSignificantBits)
                            .putLong(lost.leastSignificantBits).array()
                        controls?.publishHelper(bytes, false, false, false, 0uL)
                    }
                    enqueue { helperEvent(event) }
                }
                else -> enqueue { helperEvent(event) }
            }
        }
        scope.launch {
            com.hachiwii.oink.recovery.RecoveryController.status.collect {
                if (it.detail == "helper_attached" || !com.hachiwii.oink.recovery.RecoveryController.enabled(context)) refreshHelper()
            }
        }
        refreshHelper()
    }
    fun clipboardFacts(read: Boolean, write: Boolean) = enqueue {
        clipboardRead = read; clipboardWrite = write
        controls?.publishHelper(incarnation, state.value.phase == "ready", read, write, displayGeneration)
    }
    /** Authenticated helper attach or user unlock triggers owner health publication. */
    fun refreshHelper() = enqueue {
        if (!context.getSystemService(UserManager::class.java).isUserUnlocked || !com.hachiwii.oink.recovery.RecoveryController.enabled(context) || !PrivilegeClient.available()) {
            unavailable("helper_unavailable")
            return@enqueue
        }
        // An unlock rechecks the same helper; it must not transiently withdraw locked-screen capture.
        mutableState.value = state.value.copy(phase = "starting", reason = "", inputReady = false)
        check(PrivilegeClient.enable(true).getString("status") == "ok")
        val health = PrivilegeClient.request(Bundle().apply { putString("op", "health") })
        check(health.getString("status") == "enabled")
        val id = UUID.fromString(health.getString("incarnation"))
        helperInstance = id.toString()
        incarnation = ByteBuffer.allocate(16).putLong(id.mostSignificantBits).putLong(id.leastSignificantBits).array()
        val display = PrivilegeClient.request(Bundle().apply { putString("op", "display") }, id.toString())
        displayGeneration = display.getLong("generation").toULong()
        mutableState.value = AndroidPrivilegeState("ready", captureReady = true)
        // Read/write facts are reported only after clipboard operations establish
        // them. Binding a helper alone provides no clipboard authority.
        controls?.publishHelper(incarnation, true, false, false, displayGeneration)
        clipboard.attach(id.toString())
        configureInput(health, display)
        com.hachiwii.oink.recovery.RecoveryController.ownerReady()
    }
    private fun configureInput(health: Bundle, display: Bundle) {
        com.hachiwii.oink.recovery.RecoveryController.reportPointer(health.getString("mouse_error"))
        val absolute = health.getString("mouse_path") == "self_drawn_absolute"
        val path = AndroidPointerPath.SELF_DRAWN_ABSOLUTE
        // Helper readiness stays independent of interactive/keyguard state.
        val eligible = health.getBoolean("input_ready") && health.getBoolean("keyboard_uhid") &&
            (android.os.Build.VERSION.SDK_INT >= 30 && absolute) &&
            display.getString("status") == "display"
        val facts = if (eligible) AndroidDisplayState(display.getInt("width").toUInt(), display.getInt("height").toUInt(),
            (display.getInt("rotation") * 90).toUShort(), display.getLong("generation").toULong()) else null
        input.configure(facts, helperInstance)
        routedInput?.configure(facts, path, incarnation)
        if (facts == null) setInputReady(false)
    }
    private suspend fun helperEvent(event: Bundle) {
        if (event.getString("incarnation") != helperInstance) return
        input.event(event)
        when (event.getString("status")) {
            "ipc_lost" -> unavailable("ipc_lost")
            // Platform clipboard restrictions may change on keyguard transitions.
            // Input facts and the active keyboard remain untouched.
            "clipboard_state_changed" -> clipboard.changed(false)
            "display", "input_state_changed" -> {
                val health = PrivilegeClient.request(Bundle().apply { putString("op", "health") }, helperInstance)
                val display = PrivilegeClient.request(Bundle().apply { putString("op", "display") }, helperInstance)
                displayGeneration = display.getLong("generation").toULong()
                configureInput(health, display)
                controls?.publishHelper(incarnation, true, clipboardRead, clipboardWrite, displayGeneration)
                clipboard.changed(false)
            }
        }
    }
    private fun unavailable(reason: String) {
        mutableState.value = AndroidPrivilegeState("unavailable", reason)
        com.hachiwii.oink.recovery.RecoveryController.reportPointer(null)
        inputReadyChanged(false)
        com.hachiwii.oink.recovery.RecoveryController.ownerUnavailable()
        clipboard.detach()
        try { controls?.publishHelper(incarnation, false, false, false, displayGeneration) } catch (_: Exception) { }
        input.configure(null)
        routedInput?.configure(null, AndroidPointerPath.SELF_DRAWN_ABSOLUTE, incarnation)
    }
    /** Rust already checked the caller's link and the supported resource. */
    private suspend fun activate(request: AndroidActivationRequest) {
        if (controls?.accept(request) != true) return
        val requestKey = key(request.owner)
        requests[requestKey] = request
        proceed(requestKey)
    }
    /**
     * Opens the requested resources at once, also on a locked phone (user 2026-11-12): playback
     * and a microphone foreground service run behind the keyguard. A missing microphone
     * permission is refused, not prompted: there is no window.
     */
    private suspend fun proceed(requestKey: String) {
        val activation = requests[requestKey] ?: return
        if (requestKey in started) return
        if (activation.owner.resources.toInt() and 2 != 0 &&
            ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            controls?.finish(activation.owner, MobileMediaFailure.PERMISSION_REQUIRED)
            requests.remove(requestKey)
            return
        }
        controls?.reportProgress(activation.owner, AndroidActivationProgress.STARTING)
        started.add(requestKey)
        try {
            withContext(Dispatchers.Main.immediate) {
                startResources(activation) { controls?.activate(activation.owner) == true }
            }
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            controls?.finish(activation.owner, MobileMediaFailure.SYSTEM_BLOCKED)
            android.util.Log.w("OinkPrivilege", "media activation failed: ${e.javaClass.simpleName}")
        }
        reconcile(media.snapshot())
    }
    fun mediaChanged() = enqueue { reconcile(media.snapshot()) }
    private fun reconcile(state: MobileMediaState) {
        if (state.pauseMask.toInt() != 0) {
            requests.keys.filter { it !in started }.forEach { requests.remove(it) }
        }
        for (requestKey in started.toList()) {
            val activation = requests[requestKey]
            if (activation == null) {
                started.remove(requestKey); reportedReady.remove(requestKey)
                continue
            }
            val owner = state.remoteOwners.firstOrNull { key(it) == requestKey }
            if (owner == null || (state.pauseMask.toInt() != 0 && requestKey !in reportedReady)) {
                if (owner != null) controls?.finish(owner, MobileMediaFailure.NONE)
                if (state.stopMask.toInt() and activation.owner.resources.toInt() == 0) {
                    started.remove(requestKey); requests.remove(requestKey); reportedReady.remove(requestKey)
                }
            } else if (state.runningMask.toInt() and owner.resources.toInt() == owner.resources.toInt()) {
                reportedReady.add(requestKey)
            } else {
                val failure = (0..2).firstNotNullOfOrNull { index ->
                    state.failures[index].takeIf {
                        owner.resources.toInt() and (1 shl index) != 0 && it != MobileMediaFailure.NONE
                    }
                } ?: MobileMediaFailure.NONE
                if (failure != MobileMediaFailure.NONE) controls?.finish(owner, failure)
            }
        }
    }
    suspend fun close() {
        com.hachiwii.oink.recovery.RecoveryController.ownerUnavailable()
        PrivilegeClient.setListener(null)
        context.unregisterReceiver(unlock)
        controls?.shutdown()
        subscription?.cancel()
        queue.close()
        scope.coroutineContext[Job]?.cancelAndJoin()
        routedInput?.shutdown()
        routedInput?.destroy()
        routedInput = null
        clipboard.close()
        input.close()
        if (PrivilegeClient.available()) withContext(Dispatchers.IO) { PrivilegeClient.enable(false) }
        controls?.destroy(); subscription?.destroy()
    }
}
