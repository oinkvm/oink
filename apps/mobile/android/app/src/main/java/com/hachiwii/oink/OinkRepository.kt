package com.hachiwii.oink

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.SystemClock
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import java.io.File
import uniffi.oink_mobile_ffi.MobileEndpointState
import uniffi.oink_mobile_ffi.MobileMediaKind
import uniffi.oink_mobile_ffi.MobileMediaState
import uniffi.oink_mobile_ffi.MobileMicrophonePermission

/** This phone's own speaker/microphone (the top-bar switches, requirement 3). */
data class LocalAudio(
    val speakerOn: Boolean = false,
    val microphoneOn: Boolean = false,
    val forwardOn: Boolean = false,
    val forwardRunning: Boolean = false,
    val mediaGeneration: ULong = 0u,
    val forwardFailure: uniffi.oink_mobile_ffi.MobileMediaFailure = uniffi.oink_mobile_ffi.MobileMediaFailure.NONE,
    val forward: MobileEndpointState? = null,
    /** Starting / Active (exit playing, mic sending) / Failed, from this phone's own card. */
    val speaker: MobileEndpointState? = null,
    val microphone: MobileEndpointState? = null,
    val microphonePermission: MobileMicrophonePermission = MobileMicrophonePermission.NOT_DETERMINED,
    /** Merged with the group's audio configuration (core `audio_joined`); until then a switch only waits. */
    val joined: Boolean = false,
) {
    /**
     * This phone holds the speaker-exit, forwarding or microphone-sender role: a switch is on and the phone has
     * joined the group's audio configuration (the core turns a switch off when another device took
     * the role). Drives the audio part of the Wi-Fi locks ([followAudioRole]).
     */
    val audioRole: Boolean get() = joined && (speakerOn || microphoneOn || forwardOn)
}

/** Download (received) or upload (own message) progress of one record. */
data class TransferProgress(val bytes: Long, val total: Long?) {
    val percent: Int? get() = total?.takeIf { it > 0 }?.let { (bytes * 100 / it).toInt().coerceIn(0, 100) }
}

/** One folder listing page set, keyed by folder message and directory node (0 = folder root). */
data class FolderPageKey(val folderMessageId: Long, val directory: Long)
/** Why a folder listing ended without a page: the folder view's text, and whether 重试 can help. */
data class FolderFailure(val text: String, val retryable: Boolean)
data class DirectoryPageState(val entries: List<DirectoryEntry>, val nextOffset: Int?, val nextVersion: Long? = null)

/** One-shot pairing result for the toast; clear it with [OinkRepository.clearPairingNotice]. */
sealed interface PairingNotice {
    data class Completed(val peer: PairPeer) : PairingNotice
    data class Failed(val failure: PairFailure) : PairingNotice
}

data class AppState(
    val serviceEnabled: Boolean = false,
    val connected: Boolean = false,
    val status: String = "正在连接…",
    /** Effective device name (custom or system); see [deviceName] for the details. */
    val name: String = "Android",
    val deviceName: DeviceName? = null,
    val snapshot: CoreSnapshot = CoreSnapshot(),
    val received: ReceivedContent? = null,
    val draft: ShareDraft = ShareDraft(),
    val results: List<DeliveryResult> = emptyList(),
    val busy: Boolean = false,
    val receiving: Boolean = false,
    val progress: String? = null,
    val preparing: Boolean = false,
    /** System share entries by operation; each ShareActivity shows only its own (review M39). */
    val shares: Map<Long, ShareUiState> = emptyMap(),
    /** Where a transfer can go (core `TransferTargetsView`); null while no session runs. */
    val targets: uniffi.oink_mobile_ffi.TransferTargetsView? = null,
    /** Why the connection service could not start its core; null while it runs or retries (review M40). */
    val startupFailure: String? = null,
    /** Conversation, oldest first. Older pages are prepended by [OinkRepository.loadTransfers]. */
    val transferMessages: List<TransferMessage> = emptyList(),
    val transferNextCreatedMs: Long? = null,
    val transferNextId: Long? = null,
    val transferLoading: Boolean = false,
    val transferLoadFailed: Boolean = false,
    val transferSharedMore: Boolean = false,
    val transferSharedBoundary: Long? = null,
    /** Per record id (conversation records and folder children). */
    val transferProgress: Map<Long, TransferProgress> = emptyMap(),
    val folderPages: Map<FolderPageKey, DirectoryPageState> = emptyMap(),
    /** Listings that ended without a page (core `TransferDirectoryFailed`); cleared by the next request for that page. */
    val folderFailures: Map<FolderPageKey, FolderFailure> = emptyMap(),
    /** Downloaded files of a folder message (match `directoryNode` to `DirectoryEntry.id`). */
    val folderChildren: Map<Long, List<TransferMessage>> = emptyMap(),
    /** Record waiting to open after its download ("点卡片下载并打开"). */
    val openingRecord: Long? = null,
    /** Settings owned by the Rust core: null until read (audio ones need the running session). */
    val cacheBytes: Long? = null,
    val audioSettings: AudioSettings? = null,
    val localAudio: LocalAudio = LocalAudio(),
    val microphoneGain: MicrophoneGain? = null,
    val pairingNotice: PairingNotice? = null,
    /** Decision 38: OFF means no foreground service, so Oink stops whenever it leaves the screen. */
    val persistentNotification: Boolean = true,
    val privilegedReady: Boolean = false,
    val captureReady: Boolean = false,
    /** Android 11-12: after a reboot the user must turn wireless debugging on before privileged mode restarts. */
    val privilegedRebootNeedsWirelessDebugging: Boolean = false,
    /** QUIC port this device advertises (the add-device page's "本机信息"). */
    val selfPort: Int? = null,
    /** Settings 「点按通知」 and its 「打开页面」: the persistent notification's title and tap follow them. */
    val noticeTap: NoticeTap = NoticeTap.COPY,
    val noticeTab: HomeTab = HomeTab.DEVICES,
    /** A copy from the shared clipboard just succeeded: the notification says 「已复制」 for 2 s. */
    val noticeCopied: Boolean = false,
) {
    /** Show "关闭后 Oink 无法在后台工作" while this is false. */
    val worksInBackground: Boolean get() = persistentNotification
}

class OinkRepository(private val context: Context) {
    fun bootstrapDeviceId(): String? = core?.bootstrapDeviceId()
    fun bootstrapIdentityProof(nonce: String, serial: String, digest: String, enabled: Boolean, revision: String): String? = core?.bootstrapIdentityProof(nonce, serial, digest, enabled, revision)
    private val settings = LocalSettings(context)
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val mutableState = MutableStateFlow(AppState(name = SystemDeviceName.read(context),
        persistentNotification = settings.persistentNotification,
        noticeTap = settings.noticeTap, noticeTab = settings.noticeTab,
        privilegedRebootNeedsWirelessDebugging = com.hachiwii.oink.recovery.RecoveryPolicy.rebootNeedsWirelessDebugging(Build.VERSION.SDK_INT)))
    val state = mutableState.asStateFlow()

    private val ownerMutex = Mutex()
    @Volatile private var core: CoreBridge? = null
    val audioOwner get() = (core as? RustCoreBridge)?.audioOwner
    val mediaSession get() = (core as? RustCoreBridge)?.mediaSession
    val privilegeOwner get() = (core as? RustCoreBridge)?.privilegeOwner
    private var eventJob: Job? = null
    private var ownerStateJob: Job? = null
    private val sendMutex = Mutex()
    private val receiveMutex = Mutex()
    private val transferMutex = Mutex()
    private val directoryAppend = java.util.concurrent.ConcurrentHashMap<FolderPageKey, Boolean>()
    private val offlineHistory = OfflineTransferHistory(context)
    /** The session's own store while it runs; the offline connection only without a session. */
    private fun history(): TransferHistory = core?.history ?: offlineHistory
    /** The one outgoing operation (staging, waiting for its target, sending); guarded by [sendMutex]. */
    private var stagingJob: Job? = null
    private val shareIds = java.util.concurrent.atomic.AtomicLong()
    private val shareJobs = java.util.concurrent.ConcurrentHashMap<Long, Job>()
    /** Share operation whose progress the core events update. */
    @Volatile private var activeShare: Long? = null
    /** The core operation (16 bytes) of the send in flight, for 取消. */
    @Volatile private var sendingOperation: ByteArray? = null
    /** An outgoing operation holds [sendMutex] (staging, waiting for its target or sending). */
    @Volatile private var outgoingActive = false
    private var sendStartedAtNanos: Long? = null
    private var lastProgressAtNanos: Long? = null
    private var lastProgressBytes = 0L
    private var lastProgressTarget: String? = null
    var foreground = false
        private set
    /** Started activities (main thread); see [activityStarted]. */
    private var visibleActivities = 0
    /** A picker or camera Oink launched for the composer is on screen (main thread, review H18). */
    private var picking = false
    private var pickTarget: String? = null
    /** Set by the running ConnectionService: start the core again after a startup failure (review M40). */
    var onRetryConnection: (() -> Unit)? = null
    /** Set by the running ConnectionService: the core's network failed; restart it after the delay (ms). */
    var onNetworkFailed: ((Long) -> Unit)? = null
    /** Serializes ConnectionService instances: a new one starts only after the previous one tore down. */
    internal val serviceLock = Mutex()
    /**
     * Content started arriving straight from a phone (core `TransferDirectIncoming`): the running
     * ConnectionService posts a notification while Oink is in the background. Arguments: sender
     * name, item count, first file name, the first record (the transfer page opens at it).
     */
    var onDirectIncoming: ((String, Int, String, Long) -> Unit)? = null
    /** A hidden share's progress changed (or it ended): the ConnectionService updates its notification. */
    var onHiddenShare: ((Long, ShareUiState?) -> Unit)? = null
    var onRefreshNearby: (() -> Unit)? = null
    var onAdvertisement: ((Advertisement) -> Unit)? = null
    // Assigned by the UniFFI adapter. Missing bindings must never look like a running core.
    var createCore: (() -> CoreBridge)? = null

    // Initialization starts IO work that can report failure immediately. Keep
    // its notice channel ready before launching either history read.
    private val noticeFlow = MutableSharedFlow<String>(extraBufferCapacity = 32)
    /** One-shot messages for the UI toast: failed operations with the core's reason, and results
     * the user has to see ("没有可以打开 X 的应用"). [status] stays the notification line. */
    val notices: SharedFlow<String> = noticeFlow.asSharedFlow()

    init {
        // History and store-owned settings are readable without a running session.
        loadTransfers(reset = true)
        scope.launch {
            runCatching {
                val bytes = history().cachedBytes()
                mutableState.update { it.copy(cacheBytes = bytes) }
            }.onFailure { fail(it.oinkMessage ?: "无法读取传输设置") }
        }
    }

    fun loadOffline() = scope.launch {
        runCatching { ownerMutex.withLock { if (core == null) readOffline(false) } }
            .onFailure { fail(it.oinkMessage ?: "无法读取设备组") }
    }
    /**
     * Leaves the device group; returns null, or the failure reason ("退出设备组" waits for the outcome).
     * Runs in the repository scope so closing the page that asked does not cut it short.
     */
    suspend fun leaveGroup(): String? = scope.async {
        runCatching { ownerMutex.withLock {
            val active = core
            if (active != null) active.leaveGroup() else {
                readOffline(true)
                com.hachiwii.oink.recovery.RecoveryController.forgetGroup(context)
            }
        } }.fold(onSuccess = { null }, onFailure = { it.oinkMessage ?: "无法离开设备组" })
    }.await()
    private suspend fun readOffline(leave: Boolean) {
        val directory = File(context.filesDir, "core").apply { mkdirs() }
        val offline = uniffi.oink_mobile_ffi.OfflineManagement.open(uniffi.oink_mobile_ffi.OfflineConfig(
            directory.absolutePath, SystemDeviceName.read(context), uniffi.oink_mobile_ffi.MobilePlatform.ANDROID), null)
        try {
            val snapshot = if (leave) offline.leaveGroup() else offline.snapshot()
            mutableState.update { it.copy(snapshot = CoreSnapshot(devices = snapshot.members.map { member ->
                Device(member.device, member.name, false, isSelf = member.isSelf, platform = member.os,
                    detail = if (member.isSelf) "本机 · 离线" else "离线", fingerprint = member.fingerprint)
            })) }
        } finally { withContext(NonCancellable) { offline.shutdown(); offline.destroy() } }
    }

    // ---- Connection service lifecycle (decisions 37, 38) ----

    /** Called by OinkApplication for every Activity start/stop (main thread). */
    fun activityStarted() {
        if (visibleActivities++ == 0) ensureService()
    }
    fun activityStopped(changingConfigurations: Boolean) {
        visibleActivities = (visibleActivities - 1).coerceAtLeast(0)
        stopServiceIfIdle(changingConfigurations)
    }
    /** Main thread. Without the persistent notification the service runs only while Oink is in use. */
    private fun stopServiceIfIdle(changingConfigurations: Boolean = false) {
        if (shouldStopService(visibleActivities, changingConfigurations, settings.persistentNotification, picking,
                outgoingActive)) {
            context.stopService(Intent(context, ConnectionService::class.java))
        }
    }
    /** Composer 拍摄 / 图库 / 文件: a system screen Oink opened itself is not leaving Oink (review H18). */
    fun beginPick(target: String?) {
        pickTarget = target
        picking = true
    }
    /** The picker returned; [load] stages the result, null when nothing was chosen. Main thread. */
    fun finishPick(load: (suspend () -> ShareDraft)?) {
        val target = pickTarget
        val wasPicking = picking
        pickTarget = null
        picking = false
        when {
            load == null -> Unit
            // A new process cannot know which computer the old one picked for.
            !wasPicking -> fail("选择期间 Oink 被系统关闭，请重新选择")
            else -> sendAttachments(target, load)
        }
        stopServiceIfIdle()
    }
    /**
     * Starts (or re-evaluates) the connection service. Persistent notification ON: a foreground
     * service that keeps running after Oink leaves the screen. OFF: a plain started service that
     * only runs while an Oink screen is visible; [activityStopped] stops it.
     */
    fun ensureService() {
        val intent = Intent(context, ConnectionService::class.java)
        if (settings.persistentNotification) {
            ContextCompat.startForegroundService(context, intent.putExtra(ConnectionService.EXTRA_FOREGROUND, true))
        } else if (visibleActivities > 0) {
            context.startService(intent)
        }
    }
    /** Compatibility alias for older UI callers; service startup is automatic. */
    @Deprecated("Connection service starts with the Activity")
    fun enableService() = ensureService()
    /** Settings switch "常驻通知". Must be called from a visible screen. */
    fun setPersistentNotification(enabled: Boolean) {
        settings.persistentNotification = enabled
        mutableState.update { it.copy(persistentNotification = enabled) }
        ensureService()
    }
    /** Settings 「点按通知」 (user 2026-11-11); the running service re-posts its notification from the state. */
    fun setNoticeTap(value: NoticeTap) {
        settings.noticeTap = value
        mutableState.update { it.copy(noticeTap = value) }
    }
    /** Settings 「打开页面」 of 「点按通知」 = 打开 App. */
    fun setNoticeTab(value: HomeTab) {
        settings.noticeTab = value
        mutableState.update { it.copy(noticeTab = value) }
    }
    fun serviceEnabled(value: Boolean) { mutableState.update { it.copy(serviceEnabled = value) } }
    fun status(message: String) { mutableState.update { it.copy(status = message) } }
    /** Shows [message] as the status line and as a toast. */
    fun fail(message: String) { status(message); noticeFlow.tryEmit(message) }
    /** A toast only (a confirmed result such as "已保存到相册"). */
    fun notice(message: String) { noticeFlow.tryEmit(message) }

    /** The core did not start: shown in the app until a retry succeeds (review M40). */
    fun startFailed(reason: String) {
        mutableState.update { it.copy(startupFailure = reason, status = "连接离线") }
        noticeFlow.tryEmit("连接服务没能启动：$reason")
    }
    fun connectionRetrying() { mutableState.update { it.copy(startupFailure = null, status = "正在连接…") } }
    /** User action after a startup failure ("重试"). */
    fun retryConnection() { onRetryConnection?.invoke() ?: ensureService() }

    suspend fun startCore() = ownerMutex.withLock {
        if (core != null) return@withLock
        val bridge = createCore?.invoke() ?: error("Rust 核心尚未安装，请先生成移动绑定")
        try {
            bridge.start(File(context.filesDir, "core"), SystemDeviceName.read(context))
            // 「自动写入剪贴板」 lives only in LocalSettings; every core start gets it from there.
            bridge.automaticText(settings.autoWriteClipboard)
            withContext(Dispatchers.Main) { core = bridge }
            bridge.transferForeground(foreground)
            eventJob = scope.launch { bridge.events.collect { event -> handleCoreEvent(bridge, event) } }
            ownerStateJob = scope.launch { observeOwners(bridge) }
            val audio = runCatching { bridge.audioSettings() to bridge.microphoneGain() }
                .onFailure { fail(it.oinkMessage ?: "无法读取音频设置") }.getOrNull()
            mutableState.update { it.copy(connected = true, startupFailure = null, status = "已连接局域网", deviceName = bridge.localDeviceName(),
                name = bridge.localDeviceName().name, audioSettings = audio?.first, microphoneGain = audio?.second,
                targets = runCatching { bridge.transferTargets() }.getOrNull()) }
        } catch (e: Exception) {
            withContext(NonCancellable) { bridge.stop() }
            throw e
        }
    }

    private suspend fun observeOwners(core: CoreBridge) {
        val bridge = core as? RustCoreBridge ?: return
        val audio = bridge.audioOwner ?: return
        coroutineScope {
            launch(Dispatchers.Main.immediate) {
                combine(audio.state, state.map { s -> s.snapshot.devices.firstOrNull { it.isSelf }?.mobileAudio }.distinctUntilChanged()) { media, self ->
                    localAudio(media, self)
                }.collect { value ->
                    val before = state.value.localAudio
                    mutableState.update { it.copy(localAudio = value) }
                    val currentAttempt = forwardAttemptGeneration?.let { value.mediaGeneration >= it } == true
                    val completedAttempt = currentAttempt && (value.forwardRunning || value.forwardFailure != uniffi.oink_mobile_ffi.MobileMediaFailure.NONE)
                    val messages = audioFeedback(before, value, completedAttempt)
                    if (completedAttempt) forwardAttemptGeneration = null
                    if (visibleActivities > 0) messages.forEach(::notice)
                    else withContext(Dispatchers.Main) {
                        // Android queues distinct text Toast instances in order, including tile actions
                        // while Oink has no visible Activity. No app overlay is added to the system shade.
                        messages.forEach { android.widget.Toast.makeText(context, it, android.widget.Toast.LENGTH_SHORT).show() }
                    }
                    if (value.forwardFailure != uniffi.oink_mobile_ffi.MobileMediaFailure.NONE && (value.forwardFailure != before.forwardFailure || completedAttempt)) {
                        pendingForwardHaptic = false
                        speakerHaptics.tryEmit(false)
                    }
                    if (pendingForwardHaptic && value.forwardRunning && !before.forwardRunning) {
                        pendingForwardHaptic = false
                        speakerHaptics.tryEmit(true)
                    }
                    if (!value.forwardOn && (forwardAttemptGeneration == null || currentAttempt)) {
                        pendingForwardHaptic = false
                        forwardAttemptGeneration = null
                    }
                }
            }
            bridge.privilegeOwner?.state?.let { privileged ->
                launch { privileged.collect { value -> mutableState.update { it.copy(privilegedReady = value.phase == "ready", captureReady = value.captureReady) } } }
            }
        }
    }

    private suspend fun handleCoreEvent(bridge: CoreBridge, event: CoreEvent) {
        when (event) {
            is CoreEvent.Advertise -> {
                mutableState.update { it.copy(selfPort = event.value.port) }
                onAdvertisement?.invoke(event.value)
            }
            is CoreEvent.Snapshot -> mutableState.update { it.copy(snapshot = event.value) }
            // No "has copyable content" state any more (user 2026-11-11): the notification's title is fixed.
            is CoreEvent.Available -> Unit
            is CoreEvent.Received -> publishReceived(bridge, event.content)
            is CoreEvent.Progress -> {
                val now = SystemClock.elapsedRealtimeNanos()
                val current = state.value
                val rate = if (current.busy && lastProgressTarget == event.target &&
                    lastProgressAtNanos != null && event.bytes >= lastProgressBytes
                ) {
                    val elapsedNanos = now - lastProgressAtNanos!!
                    if (elapsedNanos > 0L) (event.bytes - lastProgressBytes) * 1_000_000_000.0 / elapsedNanos else null
                } else null
                if (current.busy) {
                    lastProgressTarget = event.target
                    lastProgressBytes = event.bytes
                    lastProgressAtNanos = now
                }
                mutableState.update { it.copy(progress = "${event.bytes} / ${event.total?.toString() ?: "未知"} 字节") }
                if (current.busy) updateShare(activeShare) {
                    it.copy(
                        phase = SharePhase.SENDING,
                        status = ShareStatusText.sending(rate),
                        bytes = event.bytes,
                        totalBytes = event.total,
                        rateBytesPerSecond = rate,
                    )
                }
            }
            is CoreEvent.Transfer -> handleTransfer(bridge, event.event)
            is CoreEvent.Failure -> fail(event.message)
            is CoreEvent.PairingCompleted -> mutableState.update { it.copy(pairingNotice = PairingNotice.Completed(event.peer)) }
            is CoreEvent.PairingFailed -> mutableState.update { it.copy(pairingNotice = PairingNotice.Failed(event.failure)) }
            is CoreEvent.NameChanged -> mutableState.update { it.copy(deviceName = event.value, name = event.value.name) }
            CoreEvent.Stopped -> mutableState.update { it.copy(connected = false, status = "连接离线", targets = null) }
            is CoreEvent.NetworkFailed -> {
                android.util.Log.e("OinkNetwork", "network layer failed; restarting the core: ${event.detail}")
                status("网络出错，正在重新连接…")
                onNetworkFailed?.invoke(event.retryAfterMillis)
            }
        }
    }

    private fun handleTransfer(bridge: CoreBridge, transfer: TransferCoreEvent) {
        when (transfer) {
            is TransferCoreEvent.Announcement -> scope.launch {
                refreshTransferRecord(transfer.recordId, transfer.parentId)
                if (transfer.autoOpenReady) openReady(bridge)
            }
            is TransferCoreEvent.SendProgress -> {
                mutableState.update { current ->
                    current.copy(transferProgress = current.transferProgress +
                        transfer.recordIds.associateWith { TransferProgress(transfer.sent, transfer.total) })
                }
                if (state.value.busy) {
                    mutableState.update { it.copy(progress = "${transfer.sent} / ${transfer.total?.toString() ?: "未知"} 字节") }
                    updateShare(activeShare) {
                        it.copy(phase = SharePhase.SENDING, bytes = transfer.sent,
                            totalBytes = transfer.total, status = ShareStatusText.sending(it.rateBytesPerSecond),
                            recordId = it.recordId ?: transfer.recordIds.firstOrNull())
                    }
                }
            }
            is TransferCoreEvent.RateSample -> if (state.value.busy) updateShare(activeShare) {
                it.copy(phase = SharePhase.SENDING,
                    rateBytesPerSecond = transfer.bytesPerSecond.toDouble(),
                    status = ShareStatusText.sending(transfer.bytesPerSecond.toDouble()))
            }
            is TransferCoreEvent.DownloadProgress -> mutableState.update { current ->
                // A finished download reports its full size last: the notification goes back to the status line.
                val done = transfer.total != null && transfer.received >= transfer.total
                current.copy(progress = if (done) null else "${transfer.received} / ${transfer.total?.toString() ?: "未知"} 字节",
                    transferProgress = current.transferProgress + (transfer.recordId to TransferProgress(transfer.received, transfer.total)))
            }
            is TransferCoreEvent.DirectoryPage -> {
                val key = FolderPageKey(transfer.folderMessageId, transfer.directory)
                // A page requested with a cursor continues the listing; a first page replaces it.
                val append = directoryAppend.remove(key) == true
                mutableState.update { current ->
                    val entries = if (append) current.folderPages[key]?.entries.orEmpty() + transfer.entries else transfer.entries
                    current.copy(folderPages = current.folderPages + (key to DirectoryPageState(entries, transfer.nextOffset, transfer.nextVersion)),
                        folderFailures = current.folderFailures - key)
                }
            }
            is TransferCoreEvent.DirectoryFailed -> {
                // The folder view stops loading and shows why (with 重试 when it can help).
                // A failed first page drops what an earlier listing showed (it may be gone); a failed next page keeps it.
                val key = FolderPageKey(transfer.folderMessageId, transfer.directory)
                val append = directoryAppend.remove(key) == true
                mutableState.update { it.copy(folderPages = if (append) it.folderPages else it.folderPages - key,
                    folderFailures = it.folderFailures + (key to FolderFailure(folderFailureText(transfer.reason), transfer.retryable))) }
            }
            is TransferCoreEvent.Delivery -> {
                scope.launch {
                    transfer.recordIds.forEach { refreshTransferRecord(it, null) }
                    mutableState.update { it.copy(transferProgress = it.transferProgress - transfer.recordIds.toSet()) }
                }
                if (!state.value.busy) status(transfer.status)
            }
            is TransferCoreEvent.Error -> {
                // `detail` is diagnostic text; the record's stored availability
                // says when its source no longer has the content.
                val id = transfer.recordId
                if (id == null) fail("传输失败，请重试")
                else {
                    // A failed or cancelled download ends its progress line too.
                    mutableState.update { it.copy(transferProgress = it.transferProgress - id, progress = null,
                        openingRecord = it.openingRecord.takeUnless { open -> open == id }) }
                    scope.launch {
                        refreshTransferRecord(id, null)
                        val availability = runCatching { history().message(id)?.sourceAvailability }.getOrNull()
                        fail(availability?.let(::availabilityText) ?: "下载失败，请重试")
                    }
                }
            }
            is TransferCoreEvent.OutgoingRecorded -> scope.launch { refreshNewestTransfers() }
            is TransferCoreEvent.Targets -> {
                mutableState.update { it.copy(targets = transfer.view) }
                if (foreground && state.value.transferMessages.isEmpty()) loadTransfers(reset = true)
            }
            is TransferCoreEvent.ShareWaiting -> shareWaiting(transfer.operationId, transfer.waiting)
            // A cancelled own share, or a phone-to-phone transfer its sender cancelled: gone on both sides.
            is TransferCoreEvent.Removed -> mutableState.update { s ->
                val gone = transfer.recordIds.toSet()
                s.copy(transferMessages = s.transferMessages.filterNot { it.id in gone }, transferProgress = s.transferProgress - gone)
            }
            is TransferCoreEvent.DirectIncoming -> {
                transfer.recordIds.firstOrNull()?.let { directIncoming(transfer.senderName, transfer.itemCount, transfer.firstName, it) }
                scope.launch { transfer.recordIds.forEach { refreshTransferRecord(it, null) } }
            }
            // A phone-to-phone receive that failed or was cancelled sends no final progress.
            is TransferCoreEvent.DirectEnded -> mutableState.update { it.copy(progress = null) }
            is TransferCoreEvent.Message -> Unit
        }
    }

    suspend fun stopCore() = ownerMutex.withLock {
        val retiring = core
        // Serialize owner retirement with the synchronous main-thread clipboard publication.
        withContext(Dispatchers.Main) { core = null; pendingForwardHaptic = false; forwardAttemptGeneration = null; mutableState.update { it.copy(connected = false) } }
        eventJob?.cancelAndJoin()
        eventJob = null
        ownerStateJob?.cancelAndJoin()
        ownerStateJob = null
        retiring?.stop()
        mutableState.update { it.copy(connected = false, audioSettings = null, microphoneGain = null, localAudio = LocalAudio(),
            privilegedReady = false, captureReady = false, openingRecord = null, snapshot = it.snapshot.copy(
            // No core, no link state: a waiting send must not trust the retired owner's last view.
            devices = it.snapshot.devices.map { d -> d.copy(online = false, connection = null, actions = emptyList(), volumes = emptyList(),
                latencyMillis = null, controls = null, pause = null, detail = if (d.isSelf) "本机 · 离线" else "离线") },
            nearby = emptyList(), requests = emptyList(), pairingCode = null, pairingSession = null, focuses = emptyList(),
            exitVolume = null, exit = null, foundAddress = null, addressLookup = AddressLookup.Idle)) }
    }
    // NSD results arrive on Main in callback order and are applied there synchronously, so a
    // withdraw can never overtake the upsert it follows. Main also orders them against owner
    // retirement: stopCore clears `core` on Main before the old bridge is destroyed.
    fun networkChanged(generation: Long) { core?.networkChanged(generation) }
    /** After bindProcessToNetwork moved the process to another Network (review H17). */
    suspend fun rebindNetwork() { core?.rebindNetwork() }
    fun discovered(service: DiscoveredService) {
        runCatching { core?.discovered(service) }.onFailure { fail(it.oinkMessage ?: "发现结果更新失败") }
    }
    fun lost(key: String, generation: Long) {
        runCatching { core?.lost(key, generation) }.onFailure { fail(it.oinkMessage ?: "发现结果撤回失败") }
    }
    fun setForeground(value: Boolean) {
        if (value) audioOwner?.refreshMicrophonePermission()
        foreground = value
        scope.launch { runCatching { core?.transferForeground(value) }.onFailure { fail(it.oinkMessage ?: "剪贴板历史同步失败") } }
    }
    fun command(block: suspend (CoreBridge) -> Unit) = scope.launch {
        runCatching { block(core ?: error("连接尚未就绪，请稍候")) }.onFailure { fail(it.oinkMessage ?: "操作失败") }
    }
    private suspend fun <T> requireCore(block: suspend (CoreBridge) -> T): T = block(core ?: error("连接尚未就绪，请稍候"))
    /** Control page session; null while the core is not running. */
    fun openControl(listener: com.hachiwii.oink.control.ControlListener) = (core as? RustCoreBridge)?.openControl(listener)
    fun refreshNearby() { onRefreshNearby?.invoke(); command { it.refreshNearby() } }

    // ---- Devices and controls ----

    /** Any device-card command: mouse/keyboard/screen/clipboard modes, game mode, audio exit,
     * forwarding, microphone modes, pause, pointer lock. The result arrives as a status line. */
    fun invokeDeviceAction(action: ControlAction) {
        val token = action.token ?: return fail(unavailableText(action))
        command { it.control(token) }
    }
    /** Several card commands in order (e.g. clear the audio exit, then start forwarding). Nothing
     * is sent when one of them is unavailable. */
    fun invokeDeviceActions(actions: List<ControlAction>) {
        actions.firstOrNull { it.token == null }?.let { return fail(unavailableText(it)) }
        command { bridge -> actions.forEach { bridge.control(it.token!!) } }
    }
    /** Forwarding gain (0..200) of a speaker or microphone forwarding control. */
    fun setForwardingVolume(control: VolumeControl, volume: Int, muted: Boolean) =
        command { it.setVolume(control, volume.coerceIn(0, 200), muted) }
    /** System volume (0..100) of the group audio exit. */
    fun setExitVolume(volume: Int, muted: Boolean): Job? {
        val control = state.value.snapshot.exit?.control ?: run { fail("音频出口音量暂不可调"); return null }
        return command { it.setVolume(control, volume.coerceIn(0, 100), muted) }
    }
    suspend fun memberAddresses(device: String): List<String> = requireCore { it.memberAddresses(device) }
    /** "a.b.c.d" (port 47470) or "a.b.c.d:port"; "" clears the manual address. */
    suspend fun setMemberAddress(device: String, address: String): EditResult = requireCore { it.setMemberAddress(device, address.trim()) }
    suspend fun block(device: String): EditResult = requireCore { it.block(device) }
    suspend fun unblock(device: String): EditResult = requireCore { it.unblock(device) }
    /** Runs an edit and shows its typed outcome as a status line (screens without an inline error). */
    fun reportEdit(success: String, block: suspend (CoreBridge) -> EditResult) = command { bridge ->
        when (val result = block(bridge)) {
            EditResult.Ok -> { status(success); noticeFlow.tryEmit(success) }
            else -> fail(editMessage(result))
        }
    }
    fun removeMember(device: String) = command { it.removeMember(device) }
    fun clearPairingNotice() { mutableState.update { it.copy(pairingNotice = null) } }

    /** Sets this phone's custom name; null or blank restores the system device name. */
    fun setLocalDeviceName(name: String?) = scope.launch {
        runCatching { ownerMutex.withLock { requireCore { it.setLocalDeviceName(name?.takeIf { value -> value.isNotBlank() }) } } }
            .onFailure { fail(it.oinkMessage ?: "无法修改设备名") }
    }
    @Deprecated("Use setLocalDeviceName", ReplaceWith("setLocalDeviceName(name)"))
    fun rename(name: String) = setLocalDeviceName(name)

    /** Settings switch 「自动写入剪贴板」: stored first (the one source of truth), then pushed to a
     * running core; a core that is not running gets it from [startCore]. */
    fun setAutomatic(value: Boolean) {
        settings.autoWriteClipboard = value
        scope.launch {
            runCatching { ownerMutex.withLock { core?.automaticText(value) } }
                .onFailure { fail(it.oinkMessage ?: "无法修改自动接收设置") }
        }
    }

    // ---- This phone's speaker / microphone ----

    /** true = accepted long press; false = rejection. Collected by the visible UI. */
    val speakerHaptics = MutableSharedFlow<Boolean>(extraBufferCapacity = 8)
    private var pendingForwardHaptic = false
    private var forwardAttemptGeneration: ULong? = null
    fun speakerPressed(long: Boolean = false) {
        val audio = state.value.localAudio
        when (speakerAction(audio.speakerOn, audio.forwardOn, state.value.captureReady, settings.speakerTap, long)) {
            SpeakerAction.REJECT -> { notice("转发音频仅在特权模式可用"); speakerHaptics.tryEmit(false) }
            SpeakerAction.OFF -> { pendingForwardHaptic = false; setLocalForward(false); setLocalSpeaker(false) }
            SpeakerAction.EXIT -> { pendingForwardHaptic = false; setLocalSpeaker(true); if (long) speakerHaptics.tryEmit(audioOwner != null) }
            SpeakerAction.FORWARD -> { pendingForwardHaptic = long; setLocalForward(true) }
        }
    }
    fun setLocalForward(enabled: Boolean) {
        if (!enabled) { forwardAttemptGeneration = null; pendingForwardHaptic = false }
        if (enabled && !state.value.captureReady) {
            pendingForwardHaptic = false
            notice("转发音频仅在特权模式可用"); speakerHaptics.tryEmit(false); return
        }
        val owner = audioOwner ?: run {
            pendingForwardHaptic = false
            fail("连接尚未就绪，请稍候")
            speakerHaptics.tryEmit(false)
            return
        }
        owner.setEnabled(MobileMediaKind.FORWARD, enabled)
        // The owner's conflated UI snapshot can skip the brief clearFailure between two failed
        // attempts. Track this explicit intent's generation so the same rejection is shown again.
        if (enabled) forwardAttemptGeneration = owner.media.snapshot().generation
    }
    fun setLocalSpeaker(enabled: Boolean) {
        if (enabled) { pendingForwardHaptic = false; forwardAttemptGeneration = null }
        val owner = audioOwner ?: return fail("连接尚未就绪，请稍候")
        owner.setEnabled(MobileMediaKind.SPEAKER, enabled)
    }
    private val microphonePermissionFlow = kotlinx.coroutines.channels.Channel<Unit>(kotlinx.coroutines.channels.Channel.CONFLATED)
    /** One visible Activity asks for RECORD_AUDIO and answers [microphonePermissionResult] (review H13). */
    val microphonePermissionRequests: Flow<Unit> = microphonePermissionFlow.receiveAsFlow()
    /** Main thread (UI switch or notification action). */
    fun setLocalMicrophone(enabled: Boolean) {
        val owner = audioOwner ?: return fail("连接尚未就绪，请稍候")
        val granted = ContextCompat.checkSelfPermission(context, android.Manifest.permission.RECORD_AUDIO) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED
        if (enabled && !granted) {
            if (visibleActivities > 0 && microphonePermissionFlow.trySend(Unit).isSuccess) return
            // No screen to ask from (notification action): the intent stays off.
            fail("请在 Oink 中允许使用麦克风")
            return
        }
        owner.setEnabled(MobileMediaKind.MICROPHONE, enabled)
    }
    fun microphonePermissionResult(granted: Boolean) {
        val owner = audioOwner ?: return fail("连接尚未就绪，请稍候")
        owner.reportMicrophonePermissionResult(granted)
        // The intent is set only with the permission (the core's own check is the protocol
        // block's part of H13).
        if (granted) owner.setEnabled(MobileMediaKind.MICROPHONE, true)
        else fail("没有麦克风权限，无法开启麦克风")
    }
    /**
     * A tile opened Oink to set this phone's speaker or microphone to [enabled] (an explicit state, not a
     * toggle): applied once the core runs. A core that failed to start already shows its failure.
     */
    fun setLocalAudioWhenReady(kind: MobileMediaKind, enabled: Boolean) {
        scope.launch {
            val ready = state.first { it.connected || it.startupFailure != null }
            if (!ready.connected) return@launch
            withContext(Dispatchers.Main) {
                when (kind) {
                    MobileMediaKind.SPEAKER -> setLocalSpeaker(enabled)
                    MobileMediaKind.MICROPHONE -> setLocalMicrophone(enabled)
                    MobileMediaKind.FORWARD -> setLocalForward(enabled)
                }
            }
        }
    }
    fun toggleSpeaker() { setLocalSpeaker(!state.value.localAudio.speakerOn) }
    fun toggleMicrophone() { setLocalMicrophone(!state.value.localAudio.microphoneOn) }
    /** This phone's microphone gain (0..200) while it sends. */
    fun setMicrophoneGain(volume: Int, muted: Boolean) = command { bridge ->
        val value = bridge.setMicrophoneGain(volume, muted)
        mutableState.update { it.copy(microphoneGain = value) }
    }

    // ---- Settings owned by the core ----

    fun setAudioIdleTimeout(kind: MobileMediaKind, value: AudioTimeout) = command { bridge ->
        bridge.setAudioIdleTimeout(kind, value)
        mutableState.update { it.copy(audioSettings = bridge.audioSettings()) }
    }
    fun setEchoCancellation(enabled: Boolean) = command { bridge ->
        bridge.setEchoCancellation(enabled)
        mutableState.update { it.copy(audioSettings = bridge.audioSettings()) }
    }
    fun refreshCacheBytes() = scope.launch {
        runCatching { history().cachedBytes() }.onSuccess { bytes -> mutableState.update { it.copy(cacheBytes = bytes) } }
            .onFailure { fail(it.oinkMessage ?: "无法统计缓存") }
    }

    fun readNativeAndBroadcast() = command { it.privilegedReadAndBroadcast() }
    fun setPrivilegedAutoRead(enabled: Boolean) = command { it.privilegedAutoRead(enabled) }

    // ---- Shared clipboard copy and clipboard paste (notification tap, Quick Settings tiles) ----

    @Volatile private var copying: Deferred<CopyOutcome>? = null
    private var copiedFlash: Job? = null
    /**
     * Copies the group's current shared clipboard (text or an image, core `receive_current_clipboard`)
     * to this phone: the notification tap and the 复制 tile. A copy already running is joined, not
     * started twice. Waits for the core while it is still starting (the core itself then waits for the
     * session's first computer).
     */
    fun copySharedClipboard(): Deferred<CopyOutcome> = synchronized(this) {
        copying?.takeIf { it.isActive } ?: scope.async { copyNow() }.also { copying = it }
    }
    private suspend fun copyNow(): CopyOutcome {
        val ready = state.first { it.connected || it.startupFailure != null }
        val bridge = core ?: return CopyOutcome.Failed(ready.startupFailure?.let { "连接服务没能启动：$it" } ?: "连接尚未就绪，请稍候")
        return receiveMutex.withLock {
            mutableState.update { it.copy(receiving = true, progress = null) }
            try {
                val content = bridge.receive() ?: return@withLock CopyOutcome.Nothing
                if (publishReceived(bridge, content)) { flashCopied(); CopyOutcome.Copied }
                else CopyOutcome.Failed(state.value.status)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                val message = e.oinkMessage ?: "复制失败"
                fail(message)
                CopyOutcome.Failed(message)
            } finally {
                mutableState.update { it.copy(receiving = false, progress = null) }
            }
        }
    }
    /** The notification title says 「已复制」 for 2 s (user 2026-11-11). */
    private fun flashCopied() {
        copiedFlash?.cancel()
        mutableState.update { it.copy(noticeCopied = true) }
        copiedFlash = scope.launch {
            delay(2000)
            mutableState.update { it.copy(noticeCopied = false) }
        }
    }
    /**
     * Privileged mode's 粘贴 tile: this phone's clipboard text, read through the helper now (one Binder
     * call; call it off the main thread). Not gated by 「自动读取剪贴板并广播」 (user 2026-11-11): that switch only
     * decides whether copies are sent by themselves; a manual paste always sends. Null when the helper
     * cannot give it as text (image, too large, locked, helper not running): the tile then opens the
     * transfer page, which reads the clipboard itself.
     */
    fun privilegedPasteText(): ClipPaste.Text? = core?.privilegedPasteText()
    /**
     * Sends this phone's clipboard once, by the transfer page's rules ([pasteTarget], [sendRoute]): text
     * is broadcast to every online device, images go to the composer's device. Empty or unsupported
     * content only shows its prompt. [done] gets null once it was sent, else why not.
     */
    fun pasteClipboard(clip: ClipPaste, done: ((String?) -> Unit)? = null) {
        val draft: suspend () -> ShareDraft = when (clip) {
            ClipPaste.Empty -> { notice(PASTE_EMPTY); done?.invoke(PASTE_EMPTY); return }
            ClipPaste.Unsupported -> { notice(PASTE_UNSUPPORTED); done?.invoke(PASTE_UNSUPPORTED); return }
            is ClipPaste.Text -> suspend { ShareDraft(clip.text, sensitive = clip.sensitive, html = clip.html) }
            is ClipPaste.Images -> suspend { ShareStaging.copy(context, clip.uris).copy(sensitive = clip.sensitive) }
        }
        scope.launch {
            // The target needs the core's first connect round, as on the page (「正在查找设备…」).
            val ready = state.first { it.startupFailure != null || (it.connected && it.targets?.searching == false) }
            if (!ready.connected) {
                val message = "连接服务没能启动：${ready.startupFailure}"
                fail(message)
                done?.invoke(message)
                return@launch
            }
            val target = pasteTarget(ready.targets, ready.snapshot.devices, settings.transferTarget, clip is ClipPaste.Text)
            withContext(Dispatchers.Main) {
                outgoing(null, target, autoProxy = false, wait = true, composer = false, done = done, load = draft)
            }
        }
    }

    // ---- Transfer history ----

    /** reset = first page (newest messages); otherwise prepends the next older page. */
    fun loadTransfers(reset: Boolean = false) = scope.launch {
        if (state.value.transferLoading) return@launch
        if (!reset && state.value.transferMessages.isNotEmpty() && state.value.transferNextCreatedMs == null && !history().hasSharedMore()) return@launch
        mutableState.update { it.copy(transferLoading = true, transferLoadFailed = false) }
        runCatching {
            transferMutex.withLock {
                if (history().hasSharedMore()) history().loadSharedOlder()
                val page = if (reset) history().page() else history().page(state.value.transferNextCreatedMs, state.value.transferNextId)
                mutableState.update { current -> current.copy(
                    transferMessages = mergeTransferPage(current.transferMessages, page.messages, reset),
                    transferNextCreatedMs = page.nextCreatedMs,
                    transferNextId = page.nextId,
                    transferSharedMore = history().hasSharedMore(), transferSharedBoundary = history().sharedBoundaryMs(),
                ) }
            }
        }.onFailure { mutableState.update { it.copy(transferLoadFailed = true) } }
        mutableState.update { it.copy(transferLoading = false) }
    }
    private suspend fun refreshTransferRecord(id: Long, parentId: Long?) {
        runCatching {
            transferMutex.withLock {
                val folder = parentId ?: history().message(id)?.parentId
                if (folder != null) {
                    val children = history().folderChildren(folder)
                    mutableState.update { it.copy(folderChildren = it.folderChildren + (folder to children),
                        transferProgress = finishedProgress(it.transferProgress, children)) }
                } else {
                    val message = history().message(id)
                    mutableState.update { it.copy(transferMessages = upsertTransfer(it.transferMessages, id, message), transferSharedMore = history().hasSharedMore(), transferSharedBoundary = history().sharedBoundaryMs(),
                        transferProgress = finishedProgress(it.transferProgress, listOfNotNull(message))) }
                }
            }
        }.onFailure { fail(it.oinkMessage ?: "无法读取传输记录") }
    }
    private fun finishedProgress(progress: Map<Long, TransferProgress>, records: List<TransferMessage>) =
        progress - records.filter { it.localPath != null && !it.isOutgoing }.map { it.id }.toSet()
    /** Own sends are stored by the core before upload starts; pick them up from the newest page. */
    private suspend fun refreshNewestTransfers() {
        runCatching {
            transferMutex.withLock {
                val page = history().page()
                mutableState.update { current ->
                    current.copy(transferMessages = page.messages.fold(current.transferMessages) { list, message -> upsertTransfer(list, message.id, message) })
                }
            }
        }.onFailure { fail(it.oinkMessage ?: "无法读取传输记录") }
    }
    fun loadFolderChildren(folderId: Long) = scope.launch { refreshTransferRecord(folderId, folderId) }
    /** Deletes the message together with its file (a folder also drops its downloaded children). */
    fun deleteTransfer(message: TransferMessage) = scope.launch {
        runCatching {
            transferMutex.withLock { history().deleteMessage(message.id) }
            mutableState.update { it.copy(transferMessages = it.transferMessages.filterNot { item -> item.id == message.id },
                folderChildren = it.folderChildren - message.id, transferProgress = it.transferProgress - message.id) }
            mutableState.update { it.copy(cacheBytes = history().cachedBytes()) }
        }.onFailure { fail(it.oinkMessage ?: "无法删除消息") }
    }
    fun clearTransferCache() = scope.launch {
        clearTransferCacheNow()?.let { fail(it) }
    }
    /** Clears the cache; returns null, or the failure reason ("清理缓存" waits for the outcome). */
    suspend fun clearTransferCacheNow(): String? = withContext(Dispatchers.IO) {
        runCatching { transferMutex.withLock { history().clearCache() } }.fold(onSuccess = {
            loadTransfers(reset = true).join()
            mutableState.update { it.copy(folderChildren = emptyMap()) }
            refreshCacheBytes().join()
            null
        }, onFailure = { it.oinkMessage ?: "无法清理传输缓存" })
    }
    /** 清理传输记录; returns null, or the failure reason. The page reloads to what stayed (transfers still
     *  moving); a running session also reports the deleted records as [TransferCoreEvent.Removed]. */
    suspend fun clearTransferHistoryNow(): String? = withContext(Dispatchers.IO) {
        runCatching { transferMutex.withLock { history().clearHistory() } }.fold(onSuccess = {
            loadTransfers(reset = true).join()
            mutableState.update { it.copy(folderChildren = emptyMap()) }
            refreshCacheBytes().join()
            null
        }, onFailure = { it.oinkMessage ?: "无法清理传输记录" })
    }
    /** open = true: download if needed, then open with the system viewer ("点卡片"); false: download only. */
    fun downloadTransfer(message: TransferMessage, open: Boolean) = scope.launch {
        runCatching {
            ownerMutex.withLock { core?.download(message, open) ?: error("连接尚未就绪，请稍候") }
        }.onSuccess {
            mutableState.update { it.copy(openingRecord = if (open) message.id else it.openingRecord.takeUnless { id -> id == message.id }) }
        }.onFailure { fail(it.oinkMessage ?: "下载任务暂不可用") }
    }
    /** An image bubble appeared: fetch the PC's thumbnail when the core wants one. Failures show
     *  in the record (thumbnail state), not as a toast: the bubble is only being scrolled past. */
    fun requestThumbnail(message: TransferMessage) = scope.launch {
        runCatching {
            if (ownerMutex.withLock { core?.requestThumbnail(message.id) } == true) refreshTransferRecord(message.id, null)
        }
    }
    /** A picture's row in a computer's folder became visible: fetch its thumbnail when the core wants one
     *  (it lands on the entry's child record). Failures show in the record, not as a toast, as for image bubbles. */
    fun requestFolderThumbnail(folder: TransferMessage, entry: DirectoryEntry) = scope.launch {
        runCatching {
            val child = ownerMutex.withLock { core?.requestFolderThumbnail(folder.id, entry) }
            if (child != null) refreshTransferRecord(child, folder.id)
        }
    }
    /** Downloads (and optionally opens) one file inside a folder message. */
    fun downloadFolderFile(folder: TransferMessage, entry: DirectoryEntry, open: Boolean) = scope.launch {
        runCatching {
            val child = ownerMutex.withLock { core?.downloadFolderFile(folder.id, entry, open) ?: error("连接尚未就绪，请稍候") }
            mutableState.update { it.copy(openingRecord = if (open) child else it.openingRecord) }
            refreshTransferRecord(child, folder.id)
        }.onFailure { fail(it.oinkMessage ?: "下载任务暂不可用") }
    }
    /** Leaving the transfer page or tapping another item: drop the pending auto-open. */
    fun cancelAutoOpen() {
        mutableState.update { it.copy(openingRecord = null) }
        scope.launch { runCatching { core?.cancelAutoOpen() }.onFailure { fail(it.oinkMessage ?: "操作失败") } }
    }
    /** First page: node 0, no cursor. Next page: the previous page's nextVersion + nextOffset. */
    fun listDirectory(message: TransferMessage, node: Long = 0L, cursorVersion: Long? = null, cursorOffset: Int? = null) = scope.launch {
        val key = FolderPageKey(message.id, node)
        directoryAppend[key] = cursorOffset != null
        // A new request (first page, next page or 重试) loads again until its page or failure arrives.
        mutableState.update { it.copy(folderFailures = it.folderFailures - key) }
        runCatching {
            ownerMutex.withLock { core?.listDirectory(message, node, cursorVersion, cursorOffset) ?: error("连接尚未就绪，请稍候") }
        }.onFailure { error ->
            // The request was not even made (no session, folder gone): no page or failure event follows.
            directoryAppend.remove(key)
            mutableState.update { it.copy(folderFailures = it.folderFailures +
                (key to FolderFailure("无法获取文件夹内容：" + (error.oinkMessage ?: "请稍后重试"), retryable = true))) }
        }
    }
    /** 重试 on a failed folder page: the page that failed (the first one, or the next after the shown entries). */
    fun retryDirectory(message: TransferMessage, node: Long) {
        val page = state.value.folderPages[FolderPageKey(message.id, node)]
        if (page?.nextOffset != null) loadMoreDirectory(message, node) else listDirectory(message, node)
    }
    fun loadMoreDirectory(message: TransferMessage, node: Long) {
        val page = state.value.folderPages[FolderPageKey(message.id, node)] ?: return
        val offset = page.nextOffset ?: return
        listDirectory(message, node, page.nextVersion ?: 0L, offset)
    }
    private suspend fun openReady(bridge: CoreBridge) {
        val ready = runCatching { bridge.takeAutoOpen() }.onFailure { fail(it.oinkMessage ?: "无法打开文件") }.getOrNull() ?: return
        mutableState.update { it.copy(openingRecord = null) }
        withContext(Dispatchers.Main) { openFile(ready) }
    }
    private fun openFile(file: TransferOpen) {
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.received", File(file.path))
        val intent = Intent(Intent.ACTION_VIEW).setDataAndType(uri, file.mime ?: context.contentResolver.getType(uri) ?: "*/*")
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        try { context.startActivity(intent) }
        catch (_: ActivityNotFoundException) { fail("没有可以打开 ${file.displayName} 的应用") }
    }

    fun cancelTransfers() {
        if (state.value.preparing) stagingJob?.cancel()
        else {
            core?.nativeClipboardChanged()
            command { it.cancelTransfers(); status("传输已取消") }
        }
    }
    fun copyReceived(content: ReceivedContent) {
        core?.nativeClipboardChanged()
        runCatching { ClipboardSink.copy(context, content) }.onFailure { fail(it.oinkMessage ?: "无法复制内容") }
    }
    /**
     * The rich text kept with text message [message] (user 2026-11-12): `displayHtml` is the cleaned
     * markup the bubble renders (MobileRichText: basic structure only, no scripts or remote resources;
     * open links only on tap), `html` / `rtf` what 复制 writes. Null when the message has none on this
     * phone (yet: a TransferAnnouncement of the message follows its arrival), or when its rich text keeps
     * no formatting: then it is plain text, and the menu has no 复制文本. A small store and file read.
     */
    fun richText(message: TransferMessage): uniffi.oink_mobile_ffi.MobileRichText? =
        if (message.kind.equals("text", true) && message.richTextFormat != null)
            runCatching { history().richText(message.id) }.getOrNull()
        else null
    /**
     * Transfer page long-press (ClipboardSink, with the record's clip marker): 复制 ([rich], the default)
     * writes a text message's rich text with its plain text, 复制文本 (`rich = false`) the plain text only;
     * other messages are copied the same either way. False when nothing was copied.
     */
    fun copyTransfer(message: TransferMessage, rich: Boolean = true): Boolean {
        core?.nativeClipboardChanged()
        val copied = runCatching {
            // Rich text that keeps no formatting is plain text (user 2026-11-12): 复制 then writes the plain text only.
            if (rich) ClipboardSink.copyRich(context, message, richText(message)?.takeIf { it.displayHtml.isNotBlank() })
            else ClipboardSink.copyPlain(context, message)
        }.getOrElse { fail(it.oinkMessage ?: "无法复制内容"); return false }
        if (!copied) fail("无法复制内容")
        return copied
    }
    /** 复制文本: [copyTransfer] with the plain text only. */
    fun copyTransferText(message: TransferMessage): Boolean = copyTransfer(message, rich = false)
    /** Writes [content] to this phone's clipboard; false when it was not written (the reason is shown). */
    private suspend fun publishReceived(bridge: CoreBridge, content: ReceivedContent): Boolean {
        val native = try { bridge.publishPrivileged(content) }
        catch (e: Exception) {
            bridge.reportPublish(content, false)
            fail(e.oinkMessage ?: "无法写入手机剪贴板")
            return false
        }
        return withContext(Dispatchers.Main) {
            if (core !== bridge || native == com.hachiwii.oink.clipboard.NativeClipboardWrite.Stale ||
                (native == com.hachiwii.oink.clipboard.NativeClipboardWrite.Unavailable && !bridge.validateReceived(content))) {
                bridge.reportPublish(content, false)
                fail("接收内容已过期，请重新获取")
                return@withContext false
            }
            try {
                if (native == com.hachiwii.oink.clipboard.NativeClipboardWrite.Unavailable) ClipboardSink.copy(context, content)
                bridge.reportPublish(content, true)
                mutableState.update { it.copy(received = content, status = "已接收并复制到手机剪贴板") }
                true
            } catch (e: Exception) {
                bridge.reportPublish(content, false)
                fail(e.oinkMessage ?: "无法写入手机剪贴板")
                false
            }
        }
    }
    // ---- Outgoing content (reviews H18, L18, M36, M39) ----
    //
    // Every send is one operation: stage its own content, wait for its target, send, then delete
    // its own staging. Attachments never pass through the composer draft, and a system share
    // reports into its own entry of [AppState.shares].

    /**
     * System share entry (ShareActivity) after the user picked [target] (native-rewrite.md 「分享 /
     * 打开方式先选设备」): a computer forwards to the group, a phone gets it straight. Returns the
     * operation whose card that Activity shows.
     */
    fun startShare(target: String, to: String, toPhone: Boolean, summary: String, waitForTarget: Boolean = false,
                   load: suspend () -> ShareDraft): Long {
        val id = shareIds.incrementAndGet()
        mutableState.update { it.copy(shares = it.shares + (id to ShareUiState(SharePhase.PREPARING, "正在准备分享…", to = to, toPhone = toPhone, summary = summary))) }
        outgoing(id, target = target, autoProxy = false, wait = true, composer = false, waitForTarget = waitForTarget, label = to, load = load)
        return id
    }
    /** The ShareActivity of [id] is finishing: it no longer shows the card. An operation that has not
     * reached the core yet is dropped with it; one that is sending completes into the transfer log.
     * A hidden share stays until it ends (its notification shows it). */
    fun forgetShare(id: Long) {
        val share = state.value.shares[id] ?: return
        if (share.hidden && (share.phase == SharePhase.SENDING || share.phase == SharePhase.WAITING)) return
        if (share.phase == SharePhase.PREPARING) shareJobs[id]?.cancel()
        // Nothing was sent while it waited for its offline target: closing the card ends it, it does not
        // start unseen later (a hidden card keeps it, with its notification).
        if (share.phase == SharePhase.WAITING) cancelShare(id)
        mutableState.update { it.copy(shares = it.shares - id) }
    }
    /** The devices last seen online (written by every session of this phone), for the share picker to show at once. */
    fun cachedTargets(): List<uniffi.oink_mobile_ffi.TransferTarget> =
        runCatching { uniffi.oink_mobile_ffi.cachedTransferTargets(File(context.filesDir, "core").path) }.getOrDefault(emptyList())
    /** Whether [device] is online: the core dials it now (waiting for the session if it is still starting). */
    suspend fun confirmOnline(device: String): Boolean {
        state.first { it.connected || it.startupFailure != null }
        val bridge = core ?: return false
        return runCatching { bridge.confirmOnline(device) }.getOrDefault(false)
    }
    /**
     * Content started arriving straight from a phone (core `TransferDirectIncoming`): notified while
     * Oink is in the background; a tap opens the transfer page at [recordId].
     */
    fun directIncoming(sender: String, count: Int, firstName: String, recordId: Long) {
        if (!foreground) onDirectIncoming?.invoke(sender, count, firstName, recordId)
    }
    /** 隐藏 on the share card: the share goes on in the background, its progress in a notification. */
    fun hideShare(id: Long) {
        updateShare(id) { it.copy(hidden = true) }
        state.value.shares[id]?.let { onHiddenShare?.invoke(id, it) }
    }
    /**
     * 取消 on the share card (or its notification): one send runs at a time ([sendMutex]), so this
     * is the operation in flight. Both ends stop; the sender's records go away (contract 「取消」).
     */
    fun cancelShare(id: Long) {
        val share = state.value.shares[id] ?: return
        if (share.phase == SharePhase.PREPARING) shareJobs[id]?.cancel()
        // Waiting for its offline target: the core ends it unrecorded (the send then fails as Cancelled).
        else if (share.phase == SharePhase.SENDING || share.phase == SharePhase.WAITING) sendingOperation?.let { op -> command { it.cancelShare(op) } }
    }
    /**
     * The core waits for the offline target of the share in flight ([waiting]), or it came online and the
     * share starts now: the card (or its notification) follows. A share whose card was closed meanwhile
     * (not hidden) is not left waiting unseen.
     */
    private fun shareWaiting(operationId: ByteArray, waiting: Boolean) {
        val op = sendingOperation?.takeIf { it.contentEquals(operationId) } ?: return
        val id = activeShare ?: return
        if (state.value.shares[id] == null) {
            if (waiting) command { it.cancelShare(op) }
            return
        }
        updateShare(id) {
            if (waiting) it.copy(phase = SharePhase.WAITING, status = ShareStatusText.waitingForTarget())
            else it.copy(phase = SharePhase.SENDING, status = ShareStatusText.sending(null, it.to?.takeIf { _ -> it.toPhone }))
        }
    }
    /**
     * Long-press 取消发送 / 取消接收 of content still moving: an own share by its operation (its records
     * go away on both sides), a phone-to-phone receive by its record. Downloads are not touched.
     */
    fun cancelTransfer(message: TransferMessage) {
        if (message.isOutgoing) command { it.cancelShare(message.operationId) }
        else command { it.cancelReceive(message.id) }
    }
    /** Composer attachments picked for [target]. */
    fun sendAttachments(target: String?, load: suspend () -> ShareDraft) =
        outgoing(null, target, autoProxy = false, wait = true, composer = false, load = load)
    /** Composer text. [target]: the proxy computer; null when none is online (text may still be
     * broadcast to the other phones, DECISIONS 13). */
    fun send(target: String?) {
        val draft = ShareDraft(state.value.draft.text)
        outgoing(null, target, autoProxy = false, wait = false, composer = true) { draft }
    }
    fun setDraft(draft: ShareDraft) {
        if (state.value.busy) return
        // The composer holds text only; attachments are their own operations.
        mutableState.update { it.copy(draft = ShareDraft(draft.text), results = emptyList()) }
    }

    private fun updateShare(id: Long?, block: (ShareUiState) -> ShareUiState) {
        if (id == null) return
        mutableState.update { s -> s.shares[id]?.let { s.copy(shares = s.shares + (id to block(it))) } ?: s }
        // A hidden share has no card: its notification follows it, and it is dropped once it ends.
        val now = state.value.shares[id] ?: return
        if (!now.hidden) return
        if (now.phase == SharePhase.SUCCEEDED || now.phase == SharePhase.FAILED) {
            mutableState.update { it.copy(shares = it.shares - id) }
            onHiddenShare?.invoke(id, null)
        } else onHiddenShare?.invoke(id, now)
    }
    private fun shareFailed(id: Long?, message: String) {
        updateShare(id) { it.copy(phase = SharePhase.FAILED, status = message) }
        fail(message)
    }

    /**
     * [waitForTarget] (a share to the group's only other device): once the core runs, the share goes to
     * [target] ([label]) and the core itself waits while that device is offline; the app does not wait
     * on its own view of the device (an offline phone often shows as 「正在连接」, which kept such a share
     * waiting here forever, before the core recorded anything).
     */
    private fun outgoing(id: Long?, target: String?, autoProxy: Boolean, wait: Boolean, composer: Boolean,
                         waitForTarget: Boolean = false, label: String? = null, done: ((String?) -> Unit)? = null,
                         load: suspend () -> ShareDraft) {
        if (!sendMutex.tryLock()) {
            val busy = if (state.value.preparing) "正在读取内容，请完成后重新选择" else "正在发送，请完成后重新分享"
            shareFailed(id, busy)
            done?.invoke(busy)
            return
        }
        outgoingActive = true
        val job = scope.launch(start = CoroutineStart.LAZY) {
            var staged: ShareDraft? = null
            // For [done]: null once delivered, else why not.
            var outcome: String? = "发送失败"
            try {
                mutableState.update { it.copy(preparing = true) }
                val draft = load().also { if (!composer) staged = it }
                val route = if (waitForTarget && target != null) awaitCore(target, label ?: target) else if (wait) awaitRoute(target, draft, autoProxy) else {
                    val s = state.value
                    when (val now = sendRoute(s.connected, s.snapshot.devices, target, draft, autoProxy, s.startupFailure)) {
                        is SendRoute.Ready -> now
                        is SendRoute.Fail -> error(now.message)
                        SendRoute.Wait -> error("连接尚未就绪，请稍候")
                    }
                }
                mutableState.update { it.copy(preparing = false) }
                outcome = deliver(id, draft, route, composer, waitForTarget)
            } catch (e: CancellationException) {
                outcome = "已取消"
                updateShare(id) { it.copy(phase = SharePhase.FAILED, status = "已取消") }
                if (id == null) notice("已取消发送")
                throw e
            } catch (e: uniffi.oink_mobile_ffi.MobileException.Transfer) {
                // 取消 before the core recorded anything (e.g. while it waited for the target): no record, no error.
                if (e.reason == uniffi.oink_mobile_ffi.TransferFailure.CANCELLED) {
                    outcome = "已取消"
                    updateShare(id) { it.copy(phase = SharePhase.FAILED, status = "已取消") }
                } else {
                    val message = e.oinkMessage ?: "发送失败"
                    outcome = message
                    shareFailed(id, message)
                }
            } catch (e: Exception) {
                val message = e.oinkMessage ?: "发送失败"
                outcome = message
                shareFailed(id, message)
            } finally {
                withContext(NonCancellable) {
                    staged?.let { ShareStaging.delete(context, it) }
                    mutableState.update { it.copy(preparing = false) }
                    id?.let { shareJobs.remove(it) }
                    outgoingActive = false
                    sendMutex.unlock()
                    // The connection may have been kept only for this send (review H18).
                    withContext(Dispatchers.Main) { stopServiceIfIdle() }
                    done?.invoke(outcome)
                }
            }
        }
        stagingJob = job
        id?.let { shareJobs[it] = job }
        job.start()
    }

    /** Waits for the core and the target as events (no timer); fails at once on a definitive state. */
    private suspend fun awaitRoute(target: String?, draft: ShareDraft, autoProxy: Boolean): SendRoute.Ready =
        when (val route = state.map { sendRoute(it.connected, it.snapshot.devices, target, draft, autoProxy, it.startupFailure) }
            .first { it !is SendRoute.Wait }) {
            is SendRoute.Ready -> route
            is SendRoute.Fail -> error(route.message)
            SendRoute.Wait -> error("unreachable")
        }

    /** The core is running (or failed to start); the share's target is the core's to wait for. */
    private suspend fun awaitCore(target: String, label: String): SendRoute.Ready {
        val now = state.first { it.connected || it.startupFailure != null }
        if (!now.connected) error("连接服务没能启动：${now.startupFailure}")
        return SendRoute.Ready(target, label)
    }

    /** Returns null when every target got it, else why not. */
    private suspend fun deliver(id: Long?, draft: ShareDraft, route: SendRoute.Ready, composer: Boolean, waitForTarget: Boolean = false): String? {
        val bridge = core ?: error("连接尚未就绪，请稍候")
        sendStartedAtNanos = SystemClock.elapsedRealtimeNanos()
        lastProgressAtNanos = null
        lastProgressBytes = 0L
        lastProgressTarget = null
        activeShare = id
        mutableState.update { it.copy(busy = true, results = emptyList(), progress = null) }
        updateShare(id) { it.copy(phase = SharePhase.SENDING, status = ShareStatusText.sending(null, it.to?.takeIf { _ -> it.toPhone })) }
        try {
            val operationId = ByteArray(16).also(java.security.SecureRandom()::nextBytes)
            sendingOperation = operationId
            val results = bridge.share(draft, route.proxy, route.label, operationId, waitForTarget)
            val success = results.isNotEmpty() && results.all { result -> result.success }
            val elapsed = sendStartedAtNanos?.let { (SystemClock.elapsedRealtimeNanos() - it) / 1_000_000_000.0 }
            val failure = if (success) null
                else results.firstOrNull { result -> !result.success }?.let { result -> deliveryFailureText(result.failure) } ?: "发送失败"
            mutableState.update { it.copy(results = results) }
            updateShare(id) {
                it.copy(
                    phase = if (success) SharePhase.SUCCEEDED else SharePhase.FAILED,
                    status = failure ?: ShareStatusText.completed(elapsed ?: 0.0),
                    bytes = it.totalBytes ?: it.bytes,
                    rateBytesPerSecond = null,
                    elapsedSeconds = if (success) elapsed else null,
                )
            }
            return failure
        } finally {
            activeShare = null
            sendingOperation = null
            sendStartedAtNanos = null
            lastProgressAtNanos = null
            lastProgressTarget = null
            // The core recorded the message; the composer starts empty again.
            mutableState.update { it.copy(busy = false, progress = null, draft = if (composer) ShareDraft() else it.draft) }
        }
    }
}

/** Default Chinese text for a rejected edit; UIs may localize from the typed fields instead. */
internal fun editMessage(result: EditResult): String = when (result) {
    EditResult.Ok -> ""
    is EditResult.Rejected -> when (result.code) {
        uniffi.oink_mobile_ffi.UserFacingCode.INVALID_ADDRESS -> "IP 地址格式不正确"
        uniffi.oink_mobile_ffi.UserFacingCode.NOT_MEMBER -> "该设备不在设备组中"
        uniffi.oink_mobile_ffi.UserFacingCode.PERSISTENCE_FAILED -> "无法保存，请重试"
        else -> "操作失败"
    }
    is EditResult.BlockRejected -> when (result.code) {
        0u -> "不能屏蔽本机"
        1u -> "无法保存屏蔽列表，请重试"
        2u -> "屏蔽列表已满（最多 ${result.limit ?: 1024u} 台）"
        else -> "屏蔽列表操作失败"
    }
}

internal fun localAudio(media: MobileMediaState, self: MobileAudioState?) = LocalAudio(
    speakerOn = media.enabledMask.toInt() and 1 != 0,
    microphoneOn = media.enabledMask.toInt() and 2 != 0,
    forwardOn = media.enabledMask.toInt() and 4 != 0,
    forwardRunning = media.runningMask.toInt() and 4 != 0,
    mediaGeneration = media.generation,
    forwardFailure = media.failures[2],
    forward = self?.forward,
    speaker = self?.speaker,
    microphone = self?.microphone,
    microphonePermission = media.microphonePermission,
    joined = media.audioJoined,
)

/** Why an own message (or a share) failed, as the transfer page shows it. */
internal fun deliveryFailureText(failure: uniffi.oink_mobile_ffi.MobileDeliveryFailure?): String? = when (failure) {
    uniffi.oink_mobile_ffi.MobileDeliveryFailure.NO_TARGET -> "没有在线的电脑"
    uniffi.oink_mobile_ffi.MobileDeliveryFailure.NOT_ACCEPTED -> "对方没有响应"
    uniffi.oink_mobile_ffi.MobileDeliveryFailure.REJECTED -> "对方拒绝接收"
    uniffi.oink_mobile_ffi.MobileDeliveryFailure.PEER_FAILED -> "对方无法接收"
    uniffi.oink_mobile_ffi.MobileDeliveryFailure.INTERRUPTED -> "连接中断"
    uniffi.oink_mobile_ffi.MobileDeliveryFailure.LOCAL -> "无法读取要发送的内容"
    uniffi.oink_mobile_ffi.MobileDeliveryFailure.STORAGE_FULL -> "手机存储空间不足"
    uniffi.oink_mobile_ffi.MobileDeliveryFailure.CANCELLED -> "已取消"
    uniffi.oink_mobile_ffi.MobileDeliveryFailure.PAUSED -> "对方已暂停共享"
    uniffi.oink_mobile_ffi.MobileDeliveryFailure.PEER_DISCONNECTED -> "对方已断开"
    null -> null
}

/** Why a device-card command cannot run right now (its token is null). */
internal fun unavailableText(action: ControlAction): String = when {
    action.needsPermissions -> "请先在电脑上授予 Oink 所需权限"
    else -> when (action.unavailable) {
        uniffi.oink_mobile_ffi.CardActionUnavailable.OFFLINE -> "设备不在线"
        uniffi.oink_mobile_ffi.CardActionUnavailable.UNSUPPORTED -> "对方的 Oink 版本不支持此操作"
        uniffi.oink_mobile_ffi.CardActionUnavailable.STATE_UNKNOWN -> "还没有收到对方的状态，请稍候"
        uniffi.oink_mobile_ffi.CardActionUnavailable.STATE_STALE -> "对方的状态已过期，请稍候"
        uniffi.oink_mobile_ffi.CardActionUnavailable.REMOTE_FAULT -> "对方设备出现故障"
        uniffi.oink_mobile_ffi.CardActionUnavailable.NOT_MEMBER -> "该设备不在设备组中"
        uniffi.oink_mobile_ffi.CardActionUnavailable.BUSY -> "对方正忙，请稍后再试"
        null -> "此操作暂不可用"
    }
}
