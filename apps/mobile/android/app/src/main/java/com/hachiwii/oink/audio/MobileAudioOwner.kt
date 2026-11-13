package com.hachiwii.oink.audio

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.database.ContentObserver
import android.media.*
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import androidx.core.content.ContextCompat
import androidx.core.content.edit
import com.hachiwii.oink.AUDIO_MICROPHONE
import com.hachiwii.oink.AUDIO_PLAYBACK
import com.hachiwii.oink.ConnectionService
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import uniffi.oink_mobile_ffi.*

data class MobileAudioCapabilities(
    val microphoneSend: Boolean,
    val speakerReceive: Boolean,
)

/** The Rust media session owns intent and the shared media/audio barrier. */
class MobileAudioOwner(
    context: Context,
    val media: MobileMediaSession,
    private val capabilitiesChanged: (MobileAudioCapabilities) -> Unit = {},
) {
    private val context = context.applicationContext
    private val permissions = this.context.getSharedPreferences("audio-permissions", Context.MODE_PRIVATE)
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val updates = Channel<Unit>(Channel.CONFLATED)
    private val audioManager = this.context.getSystemService(AudioManager::class.java)
    private val handles = LongArray(2)
    private val generations = arrayOf(0uL, 0uL, 0uL)
    private val exclusive = booleanArrayOf(true, true)
    private val kinds = listOf(MobileMediaKind.SPEAKER, MobileMediaKind.MICROPHONE, MobileMediaKind.FORWARD)
    private var subscription: MobileMediaSubscription? = null
    private var focusHeld = false
    private val attributes = AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC).build()
    private val exitVolume = SystemExitVolume(audioManager, attributes)
    private val volumeChanges = object : ContentObserver(Handler(Looper.getMainLooper())) {
        override fun onChange(selfChange: Boolean) = media.refreshExitVolume()
    }
    private val deviceCallback = object : AudioDeviceCallback() {
        override fun onAudioDevicesAdded(addedDevices: Array<out AudioDeviceInfo>) {
            capabilitiesChanged(capabilities())
            media.refreshExitVolume()
        }

        override fun onAudioDevicesRemoved(removedDevices: Array<out AudioDeviceInfo>) {
            capabilitiesChanged(capabilities())
            media.refreshExitVolume()
        }
    }
    private val mutableState = MutableStateFlow(media.snapshot())
    val state = mutableState.asStateFlow()
    /**
     * Endpoint facts for MobilePlatformFacts: which devices exist. Microphone
     * permission is not part of it; the core reports a missing permission as the
     * endpoint direction's PermissionRequired (protocol-v2 1.1).
     */
    fun capabilities(): MobileAudioCapabilities {
        val outputs = audioManager.getDevices(AudioManager.GET_DEVICES_OUTPUTS)
        val inputs = audioManager.getDevices(AudioManager.GET_DEVICES_INPUTS)
        return MobileAudioCapabilities(
            microphoneSend = inputs.isNotEmpty(),
            speakerReceive = outputs.isNotEmpty(),
        )
    }
    private val forward = ForwardCapture(media) { generation, failure ->
        scope.launch {
            if (generations[2] == generation) media.reportFailure(MobileMediaKind.FORWARD, generation, failure)
        }
    }
    private val native = kinds.take(2).mapIndexed { index, kind -> NativeAudio { generation, error ->
        scope.launch {
            if (generations[index] == generation.toULong()) {
                if (error == -899) { // AAUDIO_ERROR_DISCONNECTED: follow system route in shared mode.
                    exclusive[index] = false
                    media.requestRebuildResources(3u)
                }
                else media.reportFailure(kind, generation.toULong(), MobileMediaFailure.UNAVAILABLE)
            }
        }
    } }
    private val focus = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
        .setAudioAttributes(attributes)
        .setWillPauseWhenDucked(true)
        .setOnAudioFocusChangeListener(focusChanged@ { change ->
            // A queued callback from an abandoned request cannot grant the next local session focus.
            if (media.snapshot().enabledMask.toInt() and 1 == 0) return@focusChanged
            when (change) {
                AudioManager.AUDIOFOCUS_GAIN -> {
                    focusHeld = true
                    media.clearFailure(MobileMediaKind.SPEAKER)
                }
                else -> {
                    focusHeld = false
                    media.reportFailure(MobileMediaKind.SPEAKER, media.snapshot().generation, MobileMediaFailure.INTERRUPTED)
                }
            }
        }, Handler(Looper.getMainLooper())).build()
    private val recording = object : AudioManager.AudioRecordingCallback() {
        override fun onRecordingConfigChanged(configs: MutableList<AudioRecordingConfiguration>) {
            if (handles[1] == 0L) return
            val id = native[1].sessionId(handles[1])
            if (configs.any { it.clientAudioSessionId == id && it.isClientSilenced })
                media.reportFailure(MobileMediaKind.MICROPHONE, generations[1], MobileMediaFailure.INTERRUPTED)
        }
    }
    suspend fun start() {
        withContext(Dispatchers.Main.immediate) {
            refreshMicrophonePermission()
            audioManager.registerAudioRecordingCallback(recording, Handler(Looper.getMainLooper()))
            audioManager.registerAudioDeviceCallback(deviceCallback, Handler(Looper.getMainLooper()))
            context.contentResolver.registerContentObserver(Settings.System.CONTENT_URI, true, volumeChanges)
            media.setExitVolumeEndpoint(exitVolume)
            scope.launch { for (ignored in updates) reconcile() }
        }
        subscription = media.subscribe(object : MobileMediaListener {
            override fun onMediaState(state: MobileMediaState) { updates.trySend(Unit) }
        })
        capabilitiesChanged(capabilities())
    }
    /** Android distinguishes first request from denial through the observed permission result. */
    fun reportMicrophonePermissionResult(granted: Boolean) {
        permissions.edit { putBoolean("microphone-requested", true) }
        if (granted) media.setMicrophonePermission(MobileMicrophonePermission.GRANTED)
        else refreshMicrophonePermission()
        capabilitiesChanged(capabilities())
    }
    fun refreshMicrophonePermission() {
        val granted = ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED
        val requested = permissions.getBoolean("microphone-requested", false)
        if (granted && !requested) permissions.edit { putBoolean("microphone-requested", true) }
        val permission = when {
            context.packageManager.isPermissionRevokedByPolicy(Manifest.permission.RECORD_AUDIO, context.packageName) ->
                MobileMicrophonePermission.RESTRICTED
            granted -> MobileMicrophonePermission.GRANTED
            requested -> MobileMicrophonePermission.DENIED
            else -> MobileMicrophonePermission.NOT_DETERMINED
        }
        if (media.snapshot().microphonePermission != permission) {
            media.setMicrophonePermission(permission)
            capabilitiesChanged(capabilities())
        }
    }
    /** Phone actions only. Remote intent is committed by the authorized Activity. */
    fun setEnabled(kind: MobileMediaKind, enabled: Boolean) {
        refreshMicrophonePermission()
        if (enabled) media.clearFailure(kind)
        media.setEnabled(kind, enabled)
    }
    /**
     * Adds the audio foreground types of [resources] (1 playback, 2 microphone) to the connection
     * service, Oink's one foreground service (user 2026-11-11). Without the persistent notification
     * there is none to add them to and nothing happens: Oink then works only while it is on screen
     * (decision 38). Throws when Android refuses the type. Also called in the background for a PC's
     * activation request (AndroidPrivilegeOwner): the service keeps the eligibility it got on screen.
     */
    suspend fun ensureForeground(resources: Int) {
        withContext(Dispatchers.Main.immediate) { ConnectionService.addAudioTypes(resources and 3) }
    }
    /** A ticket may expire while Android is bringing the foreground service up. */
    internal fun reconcileActivation() {
        updates.trySend(Unit)
    }
    private suspend fun reconcile() {
        refreshMicrophonePermission()
        var snapshot = media.snapshot()
        for (index in 0..2) {
            if (snapshot.stopMask.toInt() and (1 shl index) != 0) {
                stop(index)
                media.acknowledgeStopped(kinds[index], snapshot.ownerGenerations[index])
            }
        }
        snapshot = media.snapshot()
        // A microphone may be enabled before any receiver demands capture.
        // Acquire its while-in-use service qualification on that foreground
        // action so a later background demand can open the actual stream.
        if (snapshot.enabledMask.toInt() and 2 != 0
            && mutableState.value.enabledMask.toInt() and 2 == 0
            && snapshot.microphonePermission == MobileMicrophonePermission.GRANTED
            && snapshot.failures[1] == MobileMediaFailure.NONE
        ) {
            try { ensureForeground(2) }
            catch (_: RuntimeException) {
                media.reportFailure(MobileMediaKind.MICROPHONE, snapshot.generation, MobileMediaFailure.SYSTEM_BLOCKED)
            }
            snapshot = media.snapshot()
        }
        for (index in 0..2) {
            if (snapshot.startMask.toInt() and (1 shl index) == 0) continue
            val kind = kinds[index]
            val generation = snapshot.generation
            if (index == 2) {
                // Shell's policy capture needs neither app audio focus nor a microphone/mediaProjection FGS.
                // The existing connectedDevice service keeps the sole Rust/network owner alive.
                if (!media.claimStart(MobileMediaKind.FORWARD, generation)) continue
                generations[2] = generation
                val failure = withContext(Dispatchers.IO) { forward.start(generation) }
                if (failure != null) media.reportFailure(MobileMediaKind.FORWARD, generation, failure)
                if (failure != null || !media.acknowledgeStarted(MobileMediaKind.FORWARD, generation)) {
                    stop(2)
                    media.acknowledgeStopped(MobileMediaKind.FORWARD, generation)
                }
                continue
            }
            if (index == 1 && ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
                media.reportFailure(kind, generation, MobileMediaFailure.PERMISSION_REQUIRED)
                continue
            }
            try { ensureForeground(1 shl index) }
            catch (_: RuntimeException) {
                media.reportFailure(kind, generation, MobileMediaFailure.SYSTEM_BLOCKED)
                continue
            }
            if (index == 0 && !focusHeld) {
                focusHeld = audioManager.requestAudioFocus(focus) == AudioManager.AUDIOFOCUS_REQUEST_GRANTED
                if (!focusHeld) {
                    media.reportFailure(kind, generation, MobileMediaFailure.INTERRUPTED)
                    continue
                }
            }
            if (!media.claimStart(kind, generation)) continue
            generations[index] = generation
            val handle = withContext(Dispatchers.IO) { native[index].open(index == 1, generation.toLong(), exclusive[index]) }
            handles[index] = handle
            val attached = handle != 0L && media.attachAudio(kind, generation, native[index].address(handle).toULong())
            val started = attached && withContext(Dispatchers.IO) { native[index].start(handle) == 0 }
            if (started && index == 0) exitVolume.bind(generation, native[index].deviceId(handle))
            if (!started || !media.acknowledgeStarted(kind, generation)) {
                if (!started) media.reportFailure(kind, generation, MobileMediaFailure.UNAVAILABLE)
                stop(index)
                media.acknowledgeStopped(kind, generation)
            }
        }
        snapshot = media.snapshot()
        mutableState.value = snapshot
        if (handles[0] == 0L && focusHeld) {
            audioManager.abandonAudioFocusRequest(focus)
            focusHeld = false
        }
        // Types stay while their switch is on or their stream is open (a paused microphone keeps its
        // type for the next demand); the others are dropped. Only ensureForeground adds them.
        runCatching { ConnectionService.retainAudioTypes(audioTypesInUse(snapshot.enabledMask.toInt(), handles[0] != 0L, handles[1] != 0L)) }
            .onFailure { android.util.Log.w("OinkAudio", "dropping foreground audio types failed", it) }
    }
    private suspend fun stop(index: Int) {
        if (index == 2) {
            withContext(Dispatchers.IO) { forward.close() }
            generations[2] = 0u
            return
        }
        if (index == 0) exitVolume.retire()
        val handle = handles[index]
        handles[index] = 0
        if (handle != 0L) withContext(Dispatchers.IO) { native[index].close(handle) }
        generations[index] = 0u
    }
    /** Call only after MobileSession.shutdown has waited for the native stop acknowledgements. */
    suspend fun close() {
        subscription?.cancel()
        subscription = null
        withContext(Dispatchers.IO) { forward.close() }
        withContext(Dispatchers.Main.immediate) {
            runCatching { ConnectionService.retainAudioTypes(0) }
                .onFailure { android.util.Log.w("OinkAudio", "dropping foreground audio types failed", it) }
            audioManager.unregisterAudioRecordingCallback(recording)
            audioManager.unregisterAudioDeviceCallback(deviceCallback)
            context.contentResolver.unregisterContentObserver(volumeChanges)
            exitVolume.retire()
            media.clearExitVolumeEndpoint()
            audioManager.abandonAudioFocusRequest(focus)
            scope.cancel()
        }
    }
}

/**
 * Audio foreground types still in use ([AUDIO_PLAYBACK], [AUDIO_MICROPHONE]): a resource keeps its
 * type while its switch is on ([enabledMask] bit) or its stream is still open.
 */
internal fun audioTypesInUse(enabledMask: Int, speakerOpen: Boolean, microphoneOpen: Boolean): Int =
    (if (enabledMask and 1 != 0 || speakerOpen) AUDIO_PLAYBACK else 0) or
        (if (enabledMask and 2 != 0 || microphoneOpen) AUDIO_MICROPHONE else 0)
