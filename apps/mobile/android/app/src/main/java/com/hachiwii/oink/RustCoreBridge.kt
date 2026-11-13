package com.hachiwii.oink

import android.content.ClipboardManager
import android.database.ContentObserver
import java.io.File
import java.util.UUID
import java.security.SecureRandom
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import uniffi.oink_mobile_ffi.*

/** One adapter per LAN owner. Callbacks enqueue work and never block Rust workers. */
class RustCoreBridge(private val context: android.content.Context) : CoreBridge {
    var audioOwner: com.hachiwii.oink.audio.MobileAudioOwner? = null
        private set
    val mediaSession get() = audioOwner?.media
    var privilegeOwner: com.hachiwii.oink.privilegeowner.AndroidPrivilegeOwner? = null
        private set
    private var diagnosticsCollector: MobileDiagnosticCollector? = null
    /** Opening the collector is tried once per session; a failure is logged, not retried. */
    private var diagnosticsTried = false
    private var diagnosticJob: Job? = null
    private var stopping = false
    private var privilegedClipboard: com.hachiwii.oink.clipboard.PrivilegedClipboardAdapter? = null
    private val output = Channel<CoreEvent>(Channel.UNLIMITED)
    override val events = output.receiveAsFlow()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val incoming = Channel<MobileEvent>(Channel.UNLIMITED)
    private val discovery = MobileDiscovery()
    @Volatile private var session: MobileSession? = null
    @Volatile private var clipboard: MobileClipboard? = null
    private var clipboardManager: ClipboardManager? = null
    private var nameObserver: ContentObserver? = null
    private val nativeClipboardListener = ClipboardManager.OnPrimaryClipChangedListener {
        nativeClipboardChanged()
    }
    private var receivedDirectory = File("")
    private val serial = Mutex()
    private var snapshot = CoreSnapshot()
    private var cardsStream: String? = null
    private var cardsRevision = 0uL
    private var signalsRevision = 0uL
    private var blockedStream: String? = null
    private var blockedRevision = 0uL
    private var found: FoundPeer? = null
    private val compatibility = mutableMapOf<String, String>()
    private var queryGeneration = 0uL
    private var queryId: String? = null
    private val operations = AtomicLong()
    private val nativeRevision = AtomicLong()
    private val random = SecureRandom()
    private val deliveryWaiters = ConcurrentHashMap<String, CompletableDeferred<DeliveryResult>>()
    private val deliveryTargets = ConcurrentHashMap<String, String>()
    private val owner get() = checkNotNull(session) { "连接尚未就绪，请稍候" }
    private val content get() = checkNotNull(clipboard) { "连接尚未就绪，请稍候" }
    private fun request() = UUID.randomUUID().toString()
    private fun operation() = operations.incrementAndGet().toULong()
    private suspend fun emitSnapshot() { output.send(CoreEvent.Snapshot(snapshot)) }

    /**
     * Publish only facts observed from the native owners. They never change the
     * negotiated capabilities or drop a link (protocol-v2 1.1): audio facts feed
     * the endpoint states, privileged input feeds the I0 "controllable" flag.
     */
    private fun scheduleCapabilityRefresh() {
        scope.launch {
            runCatching { serial.withLock { refreshCapabilities() } }
                .onFailure { android.util.Log.w("OinkCapabilities", "platform facts update failed", it) }
        }
    }

    private suspend fun refreshCapabilities() {
        if (stopping) return
        val audio = audioOwner?.capabilities()
            ?: com.hachiwii.oink.audio.MobileAudioCapabilities(false, false)
        val inputReady = privilegeOwner?.state?.value?.inputReady == true
        val facts = MobilePlatformFacts(
            microphoneSend = audio.microphoneSend,
            speakerReceive = audio.speakerReceive,
            // Capture is independent of whether locked-screen input is currently controllable.
            speakerSend = privilegeOwner?.state?.value?.captureReady == true &&
                com.hachiwii.oink.privilege.PrivilegeClient.available(),
            privilegedInputReady = inputReady,
        )
        val previous = owner.platformFacts()
        owner.updatePlatformFacts(facts)
        if (!diagnosticsTried) {
            diagnosticsTried = true
            try {
                diagnosticsCollector = owner.openDiagnosticsCollector(
                    UUID.randomUUID().toString(),
                    UUID.randomUUID().toString(),
                    (64L * 1024L * 1024L).toULong(),
                    16u * 1024u,
                )
                reportDiagnostic("status", "started")
                updateTransferPeers()
                diagnosticJob = scope.launch {
                    while (isActive) {
                        delay(10_000)
                        serial.withLock { runCatching { diagnosticsCollector?.flush() } }
                    }
                }
            } catch (error: Exception) {
                android.util.Log.w("OinkDiagnostics", "collector unavailable", error)
            }
        }
        if (previous.privilegedInputReady != inputReady) {
            reportDiagnostic("privileged", if (inputReady) "privileged_enabled" else "privileged_disabled")
        }
    }

    private fun reportDiagnostic(kind: String, code: String) {
        runCatching { diagnosticsCollector?.emit(System.currentTimeMillis().coerceAtLeast(0).toULong(), kind, code, null, 1u) }
    }

    override suspend fun start(directory: File, systemName: String) {
        directory.mkdirs()
        receivedDirectory = File(directory.parentFile, "received").apply { mkdirs() }
        // PCs list this phone with its App version (DeviceFacts), the one Settings → About shows.
        val appVersion = context.packageManager.getPackageInfo(context.packageName, 0).versionName
        val config = MobileConfig(directory.absolutePath, null, null, systemName, MobilePlatform.ANDROID, SessionProfile.MOBILE,
            appVersion = appVersion)
        initializeMobileGroup(config, null)
        session = MobileSession.start(config, discovery, null)
        output.send(CoreEvent.NameChanged(owner.localDeviceName().toName()))
        audioOwner = com.hachiwii.oink.audio.MobileAudioOwner(context, owner.media()) {
            scheduleCapabilityRefresh()
        }.also { it.start() }
        com.hachiwii.oink.recovery.RecoveryController.start(context)
        clipboard = owner.clipboard()
        withContext(Dispatchers.Main.immediate) {
            // Observe revisions only. Ordinary mode never reads system clipboard contents.
            clipboardManager = context.getSystemService(ClipboardManager::class.java)
            clipboardManager?.addPrimaryClipChangedListener(nativeClipboardListener)
            // The system name can change while the session runs ("About phone" rename); a custom
            // Oink name, when set, keeps precedence inside the core.
            nameObserver =SystemDeviceName.observe(context) { name ->
                scope.launch {
                    runCatching { session?.updateSystemDeviceName(name)?.let { output.send(CoreEvent.NameChanged(it.toName())) } }
                        .onFailure { android.util.Log.w("OinkName", "system name update failed", it) }
                }
            }
        }
        val nativeClipboard = com.hachiwii.oink.clipboard.PrivilegedClipboardAdapter(content,
            ::nativeClipboardChanged, ::validateReceived) { read, write -> privilegeOwner?.clipboardFacts(read, write) }
        privilegedClipboard = nativeClipboard
        privilegeOwner = com.hachiwii.oink.privilegeowner.AndroidPrivilegeOwner(
            context, owner, owner.media(), nativeClipboard, 7,
            { activation, commit ->
                val audio = checkNotNull(audioOwner)
                var activated = false
                try {
                    audio.ensureForeground(activation.owner.resources.toInt())
                    activated = commit()
                    check(activated) { "media activation expired" }
                } finally {
                    if (!activated) audio.reconcileActivation()
                }
            },
            { scheduleCapabilityRefresh() },
        )
        privilegeOwner!!.start()
        scope.launch {
            privilegeOwner?.state?.collect { scheduleCapabilityRefresh() }
        }
        serial.withLock { refreshCapabilities() }
        nativeClipboard.setBroadcastEnabled(LocalSettings(context).autoReadClipboard)
        owner.subscribe(object : MobileEventListener {
            override fun onEvent(event: MobileEvent) { incoming.trySend(event) }
        })
        serial.withLock { refresh() }
        // Transfer and clipboard events arrive through the listener above as they happen.
        scope.launch {
            for (event in incoming) {
                try { serial.withLock { handle(event) } }
                catch (e: CancellationException) { throw e }
                catch (e: Exception) { output.send(CoreEvent.Failure(e.message ?: "核心事件处理失败")) }
            }
        }
    }
    override suspend fun stop() {
        withContext(NonCancellable + Dispatchers.Main.immediate) {
            clipboardManager?.removePrimaryClipChangedListener(nativeClipboardListener)
            clipboardManager = null
            nameObserver?.let { context.contentResolver.unregisterContentObserver(it) }
            nameObserver = null
        }
        diagnosticJob?.cancelAndJoin()
        serial.withLock {
            stopping = true
            reportDiagnostic("status", "stopped")
            runCatching { diagnosticsCollector?.flush() }
            diagnosticsCollector?.destroy()
            diagnosticsCollector = null
            diagnosticsTried = false
        }
        privilegeOwner?.close()
        privilegeOwner = null
        privilegedClipboard = null
        deliveryWaiters.values.forEach { it.cancel() }
        deliveryWaiters.clear()
        deliveryTargets.clear()
        scope.coroutineContext[Job]?.cancelAndJoin()
        val retiring = session
        session = null
        val retiringClipboard = clipboard
        clipboard = null
        retiring?.shutdown()
        audioOwner?.close()
        audioOwner = null
        retiringClipboard?.destroy()
        retiring?.destroy()
        discovery.destroy()
        incoming.close()
        output.close()
    }
    private suspend fun advertise() {
        val a = owner.advertisement()
        output.send(CoreEvent.Advertise(Advertisement(a.instance, a.serviceType, a.port.toInt(), a.txt)))
    }
    private suspend fun refresh() {
        snapshot = snapshot.copy(nearby = owner.nearby().rows.map { it.toDevice() })
        updateBlocked(owner.blockedDevices())
        updateCards(owner.deviceCards())
        updateTransferPeers()
        owner.negotiationFailures().forEach { updateCompatibility(it) }
        snapshot = snapshot.copy(focuses = owner.focusSummaries())
        updatePairing()
        advertise()
        emitSnapshot()
        owner.subscribePairingEvents(0uL).forEach { handle(MobileEvent.Pairing(it)) }
    }
    private fun updateCards(cards: DeviceCardsView) {
        if (cardsStream != null && (cardsStream != cards.stream || cards.revision < cardsRevision)) return
        if (cardsStream != cards.stream) signalsRevision = 0uL
        cardsStream = cards.stream
        cardsRevision = cards.revision
        val onlineBefore = snapshot.devices.filter { it.online && !it.isSelf }.mapTo(mutableSetOf()) { it.id }
        val devices = cards.cards.map { it.toDevice() }
        val onlineAfter = devices.filter { it.online && !it.isSelf }.mapTo(mutableSetOf()) { it.id }
        for (ignored in onlineAfter - onlineBefore) reportDiagnostic("link", "connected")
        for (ignored in onlineBefore - onlineAfter) reportDiagnostic("link", "disconnected")
        val exit = cards.exitVolume?.let {
            val token = it.versionToken
            val control = if (!it.canSet || token == null || it.volumePercent == null || it.muted == null) null
                else VolumeControl("${it.receiverName} 输出音量", token, it.volumePercent!!.toInt(), it.muted!!, VolumeKind.Output)
            ExitState(it.receiverDevice, it.receiverName, it.volumePercent?.toInt(), it.muted, control)
        }
        snapshot = snapshot.copy(devices = devices, exitVolume = exit?.control, exit = exit)
        updateTransferPeers()
    }

    private fun updateTransferPeers() {
        val peers = snapshot.devices.filterNot { it.isSelf }.mapIndexed { index, device ->
            MobilePeer(device.id, if (device.computer) "pc" else "mobile", if (device.online) "ready" else "offline",
                device.computer, index.toULong())
        }
        runCatching { diagnosticsCollector?.setPeers(peers) }.onFailure {
            android.util.Log.w("OinkDiagnostics", "collector peer update failed: ${it.message}")
        }
    }
    private fun updateBlocked(value: BlockedDevicesSnapshot) {
        if (blockedStream != null && (blockedStream != value.stream || value.revision < blockedRevision)) return
        blockedStream = value.stream
        blockedRevision = value.revision
        snapshot = snapshot.copy(blocked = value.entries.map {
            BlockedDevice(it.device, it.name ?: it.fingerprint, it.os?.lowercase() ?: "other", it.fingerprint, it.blockedAtMs.toLong())
        })
    }
    private fun updateCompatibility(value: PeerCompatibility) {
        if (value.problem == null) compatibility.remove(value.device)
        else {
            val issue = value.problem!!
            compatibility[value.device] = issue.missingCapability?.let { "对端缺少能力：$it" } ?: "对端协议不兼容：${issue.code}"
        }
        snapshot = snapshot.copy(compatibilityStatus = compatibility.map { (id, reason) ->
            "${snapshot.devices.find { it.id == id }?.name ?: id.take(8)}：$reason"
        })
    }
    private suspend fun updatePairing() {
        var pairs = owner.pairings()
        var active = pairs.sessions.lastOrNull { it.phase < 7u }
        var code: String? = null
        if (active?.direction == Direction.OUTGOING && active.phase == 1u) {
            try {
                code = owner.pairingCode(active.id).sixDigits
            } catch (e: MobileException.Command) {
                if (e.code != UserFacingCode.DEVICE_UNAVAILABLE) throw e
                // Verification may retire the code between the snapshot and this call.
                pairs = owner.pairings()
                active = pairs.sessions.lastOrNull { it.phase < 7u }
            }
        } else if (active?.direction == Direction.OUTGOING && active.phase == 3u &&
            active.id == snapshot.pairingSession?.id) {
            code = snapshot.pairingCode
        }
        snapshot = snapshot.copy(requests = pairs.requests.map { PairRequest(it.id, it.peer.name,
            it.revision.toString(), it.sourceCount.toInt(), it.targetCount.toInt(), it.peer.device, it.peer.os.lowercase()) },
            pairingCode = code, pairingSession = active?.toSession())
    }
    private suspend fun handle(event: MobileEvent) {
        when (event) {
            is MobileEvent.DeviceCards -> { updateCards(event.snapshot); emitSnapshot() }
            is MobileEvent.BlockedDevices -> {
                updateBlocked(event.snapshot)
                emitSnapshot()
            }
            is MobileEvent.DeviceCardSignals -> {
                val signals = event.signals
                if (signals.stream == cardsStream && signals.revision > signalsRevision) {
                    signalsRevision = signals.revision
                    snapshot = snapshot.copy(devices = applySignals(snapshot.devices, signals))
                    emitSnapshot()
                }
            }
            is MobileEvent.DeviceControlCompleted -> controlResult(event.result)
            is MobileEvent.Pairing -> {
                when (val pairing = event.event) {
                    is PairingEvent.AddressQueryFinished -> if (pairing.operationId == queryId && pairing.generation == queryGeneration) {
                        when (val result = pairing.result) {
                            is AddressQueryResult.Found -> {
                                found = result.peer
                                val peer = result.peer.peer
                                val device = Device(peer.device, peer.name, false, pairTarget = "query:${result.peer.queryId}",
                                    detail = peer.os, platform = peer.os.lowercase(),
                                    fingerprint = runCatching { deviceFingerprint(peer.device) }.getOrDefault(""))
                                snapshot = snapshot.copy(foundAddress = device, addressLookup = AddressLookup.Found(device))
                            }
                            is AddressQueryResult.Problem -> {
                                snapshot = snapshot.copy(foundAddress = null,
                                    addressLookup = AddressLookup.Failed(result.problem.network, result.problem.toFailure()))
                            }
                        }
                    }
                    is PairingEvent.PairingFailed -> {
                        // The pairing sheet shows the typed reason; no second, untyped failure line.
                        output.send(CoreEvent.PairingFailed(pairing.problem.toFailure()))
                        owner.ackPairingEvent(pairing.eventId)
                    }
                    is PairingEvent.PairingCompleted -> {
                        val peer = pairing.peer
                        output.send(CoreEvent.PairingCompleted(PairPeer(peer.device, peer.name, peer.os.lowercase())))
                        status("已与 ${peer.name} 配对")
                        owner.ackPairingEvent(pairing.eventId)
                        advertise()
                    }
                    is PairingEvent.PeerCompatibilityChanged -> updateCompatibility(pairing.v1)
                    is PairingEvent.MembershipEdited -> typedResult(pairing.result)
                    is PairingEvent.NearbyRefreshFinished -> typedResult(pairing.result)
                }
                snapshot = snapshot.copy(nearby = owner.nearby().rows.map { it.toDevice() })
                updatePairing()
                emitSnapshot()
            }
            is MobileEvent.Clipboard -> when (val clip = event.event) {
                is ClipboardEvent.Metadata -> output.send(CoreEvent.Available(clip.freshCopy && clip.automaticEligible))
                // Explicit receives return to their caller; events have no waiting caller.
                is ClipboardEvent.Received -> {
                    if (clip.v1.kind != ReceiveKind.EMPTY) output.send(CoreEvent.Received(clip.v1.toContent()))
                }
                is ClipboardEvent.Progress -> output.send(CoreEvent.Progress(clip.target, clip.bytes.toLong(), clip.total?.toLong()))
            }
            is MobileEvent.AdvertisementChanged -> {
                android.util.Log.i("OinkNetwork", "advertisement_changed")
                advertise()
            }
            is MobileEvent.PeerCompatibilityChanged -> { updateCompatibility(event.status); emitSnapshot() }
            is MobileEvent.FocusChanged -> { snapshot = snapshot.copy(focuses = owner.focusSummaries()); emitSnapshot() }
            is MobileEvent.TransferMessage -> output.send(CoreEvent.Transfer(TransferCoreEvent.Message(event.peerId, event.message)))
            is MobileEvent.TransferAnnouncement -> output.send(CoreEvent.Transfer(
                TransferCoreEvent.Announcement(event.recordId, event.contentId, event.parentId, event.autoOpenReady)))
            is MobileEvent.TransferDownloadProgress -> output.send(CoreEvent.Transfer(
                TransferCoreEvent.DownloadProgress(event.operationId, event.received.toLong(), event.total?.toLong(), event.recordId)))
            is MobileEvent.TransferRateSample -> output.send(CoreEvent.Transfer(
                TransferCoreEvent.RateSample(event.operationId, event.bytesPerSecond.toLong())))
            is MobileEvent.TransferSendProgress -> output.send(CoreEvent.Transfer(
                TransferCoreEvent.SendProgress(event.operationId, event.sent.toLong(), event.total?.toLong(), event.recordIds)))
            is MobileEvent.TransferDirectoryPage -> output.send(CoreEvent.Transfer(
                TransferCoreEvent.DirectoryPage(event.directory.toLong(), event.entries.map { entry ->
                    DirectoryEntry(entry.id.toLong(), entry.parent?.toLong(), entry.name, entry.kind,
                        entry.size?.toLong(), entry.version.toLong(), entry.child?.toTransferMessage())
                }, event.nextOffset?.toInt(), event.folderMessageId, event.nextVersion?.toLong())))
            is MobileEvent.TransferDirectoryFailed -> output.send(CoreEvent.Transfer(
                TransferCoreEvent.DirectoryFailed(event.folderMessageId, event.directory.toLong(), event.reason,
                    event.retryable, event.detail)))
            is MobileEvent.DeliveryEvent -> {
                reportDiagnostic("transfer", "transfer_finished")
                output.send(CoreEvent.Transfer(TransferCoreEvent.Delivery(event.operationId, event.status, event.recordIds,
                    event.deliveryState, event.failure)))
                val key = event.operationId.hexKey()
                deliveryWaiters.remove(key)?.complete(DeliveryResult(deliveryTargets.remove(key) ?: "设备",
                    event.deliveryState == MobileDeliveryState.SENT, event.status, event.failure))
            }
            is MobileEvent.TransferError -> {
                reportDiagnostic("error", "failed")
                output.send(CoreEvent.Transfer(TransferCoreEvent.Error(event.operationId, event.detail, event.recordId)))
            }
            is MobileEvent.TransferTargets -> output.send(CoreEvent.Transfer(TransferCoreEvent.Targets(event.targets)))
            is MobileEvent.TransferRemoved -> output.send(CoreEvent.Transfer(TransferCoreEvent.Removed(event.recordIds)))
            is MobileEvent.TransferDirectIncoming -> output.send(CoreEvent.Transfer(
                TransferCoreEvent.DirectIncoming(event.senderName, event.recordIds, event.itemCount.toInt(), event.firstName)))
            // The receive's records carry how it ended (announcements refresh them); the notification
            // that announced it stays, its tap shows the result.
            is MobileEvent.TransferDirectEnded -> output.send(CoreEvent.Transfer(TransferCoreEvent.DirectEnded(event.recordIds)))
            is MobileEvent.TransferShareWaiting -> output.send(CoreEvent.Transfer(TransferCoreEvent.ShareWaiting(event.operationId, event.waiting)))
            is MobileEvent.ResyncRequired -> refresh()
            is MobileEvent.Stopped -> output.send(CoreEvent.Stopped)
            // The core already recorded it in the diagnostics collector.
            is MobileEvent.NetworkFailed -> output.send(CoreEvent.NetworkFailed(event.retryAfterMs.toLong(), event.detail))
        }
    }
    private suspend fun status(text: String) {
        snapshot = snapshot.copy(managementStatus = (snapshot.managementStatus + text).takeLast(8))
        emitSnapshot()
    }
    private fun problem(value: PairProblem) = "配对失败：${pairProblemText(value.code)}"
    private suspend fun typedResult(value: TypedResult) {
        if (value is TypedResult.Problem) output.send(CoreEvent.Failure(problem(value.problem)))
    }
    private fun accepted(value: PairAcceptedReply) {
        if (value is PairAcceptedReply.Problem) error(problem((value.v1 as PairRejection.Problem).problem))
    }
    private suspend fun controlResult(value: DeviceControlCompleted) {
        val text = when (val result = value.result) {
            is DeviceControlResult.Applied -> "设置已生效"
            is DeviceControlResult.NoChange -> "设置未变化"
            is DeviceControlResult.Refused -> "设置被拒绝：${controlFailureText(result.reason)}"
            is DeviceControlResult.Failed -> "设置失败：${controlFailureText(result.reason)}"
        }
        status(text)
        // A refused or failed device command is shown to the user with the owner's reason.
        if (value.result is DeviceControlResult.Refused || value.result is DeviceControlResult.Failed) output.send(CoreEvent.Failure(text))
    }
    private suspend fun controlReply(value: DeviceControlReply) = serial.withLock {
        when (value) {
            is DeviceControlReply.Accepted -> status("设置等待确认")
            is DeviceControlReply.Completed -> controlResult(value.v1)
        }
    }
    override fun networkChanged(generation: Long) {
        discovery.networkChanged(generation.toULong())
        android.util.Log.i("OinkNetwork", "network_changed generation=$generation")
    }
    override suspend fun rebindNetwork() {
        owner.rebindNetwork()
        android.util.Log.i("OinkNetwork", "endpoint_rebound")
    }
    override fun discovered(service: DiscoveredService) {
        discovery.upsert(service.generation.toULong(), service.key, service.txt,
            service.endpoints.map { ResolvedEndpoint(it.address, it.port.toUShort(), it.scopeId.toUInt()) })
    }
    override fun lost(key: String, generation: Long) { discovery.withdraw(generation.toULong(), key) }
    override suspend fun automaticText(value: Boolean) { content.setAutoReceive(value) }
    override suspend fun privilegedReadAndBroadcast() { privilegedClipboard?.changed(true) }
    override suspend fun privilegedAutoRead(enabled: Boolean) { privilegedClipboard?.setBroadcastEnabled(enabled) }
    override suspend fun pair(target: String) {
        val pairTarget = if (target.startsWith("query:")) {
            val result = checkNotNull(found) { "地址查询已失效，请重新查找" }
            PairTarget.PreparedQuery(result.queryId, result.generation)
        } else PairTarget.NearbyDevice(target)
        val reply = owner.startPairing(request(), pairTarget)
        if (reply is PairStartedReply.Problem) error(problem((reply.v1 as PairRejection.Problem).problem))
    }
    override suspend fun accept(request: PairRequest) { accepted(owner.acceptPairing(request.id, request.revision.toULong())) }
    override suspend fun submitCode(session: PairSession, code: String) { accepted(owner.submitPairingCode(session.id, session.revision.toULong(), session.attempt, code)) }
    override suspend fun reject(request: PairRequest) { accepted(owner.declinePairing(request.id, request.revision.toULong())) }
    override suspend fun cancelPairing(): Boolean {
        val pair = snapshot.pairingSession ?: return false
        val result = owner.cancelPairing(pair.id, pair.revision.toULong())
        if (result is PairCancelReply.Problem) error(problem((result.v1 as PairRejection.Problem).problem))
        if (result is PairCancelReply.Value) when (result.v1) {
            CancelResult.ALREADY_COMMITTED -> serial.withLock { status("配对已完成") }
            CancelResult.COMMIT_UNCONFIRMED -> output.send(CoreEvent.Failure("配对提交结果待确认，请检查设备列表"))
            CancelResult.CANCELLED -> return true
        }
        return false
    }
    override suspend fun deviceSettings(device: String) = owner.deviceSettings(device)
    override suspend fun setInputTransform(device: String, value: InputTransform) { owner.setInputTransform(device, value) }
    override suspend fun setScreenGain(screen: ScreenKey, gain: Double, remote: Boolean) {
        if (remote) owner.setScreenRemoteGain(screen, gain) else owner.setScreenGain(screen, gain)
    }
    override suspend fun setScreenDisabled(device: String, disabled: Boolean) { owner.setScreenDisabled(device, disabled) }
    override suspend fun setInputOption(device: String, option: String, enabled: Boolean) {
        when (option) {
            "macScrollFeel" -> owner.setMacScrollFeel(device, enabled)
            "nativeMultifinger" -> owner.setNativeMultifinger(device, enabled)
            "touchpadInjection" -> owner.setTouchpadInjection(device, enabled)
            else -> error("未知设置")
        }
    }
    override suspend fun setFocus(value: FocusSummary, target: String?, locked: Boolean) {
        controlReply(owner.setRemoteFocus(request(), value.owner, value.versionToken, locked, target))
    }
    override suspend fun control(actionToken: String) { controlReply(owner.invokeDeviceCardAction(request(), actionToken)) }
    override suspend fun leaveGroup() { owner.leaveGroup() }
    override suspend fun refreshNearby() { accepted(owner.refreshNearby(request())) }
    override suspend fun queryAddress(address: String, port: Int?) {
        releaseAddress()
        queryGeneration++
        val id = request()
        queryId = id
        serial.withLock { snapshot = snapshot.copy(addressLookup = AddressLookup.Searching); emitSnapshot() }
        try {
            accepted(owner.queryPairingAddress(id, queryGeneration, address, port?.toUShort()))
        } catch (e: Exception) {
            serial.withLock { snapshot = snapshot.copy(addressLookup = AddressLookup.Idle); emitSnapshot() }
            throw e
        }
    }
    override suspend fun releaseAddress() {
        queryId?.let { owner.releasePairingQuery(it, queryGeneration) }
        queryId = null
        found = null
        serial.withLock { snapshot = snapshot.copy(foundAddress = null, addressLookup = AddressLookup.Idle); emitSnapshot() }
    }
    private fun blockReply(reply: BlockReply): EditResult = when (reply) {
        is BlockReply.Error -> EditResult.BlockRejected(reply.v1.code, reply.v1.limit)
        is BlockReply.Edited -> EditResult.Ok
    }
    override suspend fun block(device: String) = blockReply(owner.block(request(), device))
    override suspend fun unblock(device: String) = blockReply(owner.unblock(request(), device))
    override suspend fun renameDevice(device: String, name: String) { owner.renameDevice(device, name) }
    override suspend fun removeMember(device: String) { owner.removeMember(device) }
    override suspend fun pauseAll() { owner.pauseAll(request()) }
    override suspend fun setVolume(control: VolumeControl, volume: Int, muted: Boolean) {
        controlReply(if (control.kind == VolumeKind.Forwarding) owner.setForwardingControls(request(), control.token, volume.toUShort(), muted)
            else owner.setExitVolume(request(), control.token, volume.toUByte(), muted))
    }
    override suspend fun memberAddresses(device: String) = owner.memberAddresses(device)
    override suspend fun setMemberAddress(device: String, address: String): EditResult = try {
        owner.setMemberAddress(device, address)
        EditResult.Ok
    } catch (e: MobileException.Command) {
        EditResult.Rejected(e.code, e.detail)
    }
    override suspend fun setLocalDeviceName(name: String?): DeviceName {
        val value = owner.setLocalDeviceName(name?.trim()).toName()
        output.send(CoreEvent.NameChanged(value))
        return value
    }
    override fun localDeviceName(): DeviceName = owner.localDeviceName().toName()
    /** The control page's handle on this session (protocol-v2.md §14.6); null before the session starts. */
    fun openControl(listener: com.hachiwii.oink.control.ControlListener): com.hachiwii.oink.control.ControlLink? =
        session?.let { com.hachiwii.oink.control.RustControl.open(it, listener) }
    /** Text, or an image the core downloaded into its transfer store (it keeps the file: the clip refers to it). */
    override suspend fun receive(): ReceivedContent? {
        val revision = nativeRevision.get().toULong()
        val current = try {
            owner.receiveCurrentClipboard()
        } catch (e: MobileException.Transfer) {
            // No computer to ask, its clipboard source offline or expired: nothing to copy, not a failure.
            if (e.reason == TransferFailure.NO_TARGET || e.reason == TransferFailure.SOURCE_OFFLINE ||
                e.reason == TransferFailure.EXPIRED) return null
            throw e
        } ?: return null
        return when (current) {
            is MobileCurrentClipboard.Text -> ReceivedContent(text = current.text, sensitive = current.sensitive,
                operationId = operation(), nativeRevision = revision, marker = current.marker, html = current.html)
            is MobileCurrentClipboard.Image -> {
                val file = File(current.path)
                val mime = ClipboardSink.imageMime(file.name, current.mime) ?: error("无法识别图片格式")
                ReceivedContent(items = listOf(LocalItem(current.path, file.name, mime, current.size.toLong())),
                    sensitive = current.sensitive, operationId = operation(), nativeRevision = revision, marker = current.marker)
            }
            is MobileCurrentClipboard.ImageTooLarge -> error("图片太大，无法复制")
            is MobileCurrentClipboard.Unsupported -> null
        }
    }
    override fun privilegedPasteText(): ClipPaste.Text? = pasteTextOf(privilegedClipboard?.readForPaste())
    override fun nativeClipboardChanged() { clipboard?.setNativeRevision(nativeRevision.incrementAndGet().toULong()) }
    override fun validateReceived(value: ReceivedContent) = value.nativeRevision == nativeRevision.get().toULong() &&
        clipboard?.validateReceived(value.operationId) == true
    override suspend fun publishPrivileged(value: ReceivedContent) = privilegedClipboard?.write(value)
        ?: com.hachiwii.oink.clipboard.NativeClipboardWrite.Unavailable
    override fun reportPublish(value: ReceivedContent, success: Boolean) { clipboard?.reportPublish(value.operationId, success) }
    private fun ReceiveResult.toContent() = ReceivedContent(
        text,
        files.map { LocalItem(it.path, it.name, it.mime, File(it.path).length()) },
        sensitive,
        operationId.toULong(),
        nativeRevision.toULong(),
        marker,
        html,
    )
    override suspend fun cancelTransfers() { content.cancelAll() }
    override suspend fun cancelShare(operationId: ByteArray) { owner.transferCancelShare(operationId) }
    override suspend fun cancelReceive(messageId: Long) { owner.transferCancelReceive(messageId) }
    override fun transferForeground(value: Boolean) { owner.transferSetForeground(value) }
    override fun transferTargets(): TransferTargetsView = owner.transferTargets()
    override suspend fun confirmOnline(device: String): Boolean = owner.transferConfirmOnline(device)
    /** Waits for the core's single DeliveryEvent of this share (it always settles: sent, rejected,
     * not accepted, link lost or local failure), so long uploads are not reported early. */
    override suspend fun share(draft: ShareDraft, target: String?, label: String, operationId: ByteArray,
                               waitForTarget: Boolean): List<DeliveryResult> {
        val messageId = newTransferId()
        val items = shareItems(draft) { File(it).isDirectory }
        val key = operationId.hexKey()
        deliveryTargets[key] = label
        val waiter = CompletableDeferred<DeliveryResult>()
        deliveryWaiters[key] = waiter
        try {
            // With waitForTarget this returns only once the target was online and the share recorded.
            owner.transferSendShare(operationId, messageId, items, "transfer", target, waitForTarget,
                if (draft.sensitive) uniffi.oink_mobile_ffi.MobileShareSensitivity.CONCEALED else null)
            output.send(CoreEvent.Transfer(TransferCoreEvent.OutgoingRecorded(operationId)))
            return listOf(waiter.await())
        } finally {
            deliveryWaiters.remove(key)
            deliveryTargets.remove(key)
        }
    }

    override suspend fun download(message: TransferMessage, open: Boolean) {
        owner.transferDownload(message.id, open)
    }

    override suspend fun requestThumbnail(messageId: Long): Boolean = owner.transferRequestThumbnail(messageId)

    override suspend fun downloadFolderFile(folderMessageId: Long, entry: DirectoryEntry, open: Boolean): Long =
        owner.transferDownloadDirectoryFile(newTransferId(), folderMessageId, entry.toCore(), open)

    override suspend fun requestFolderThumbnail(folderMessageId: Long, entry: DirectoryEntry): Long? =
        owner.transferRequestDirectoryThumbnail(folderMessageId, entry.toCore())

    override suspend fun listDirectory(message: TransferMessage, node: Long, cursorVersion: Long?, cursorOffset: Int?) {
        // Every request carries its own operation id; pages are matched by folder message and node.
        owner.transferListDirectory(newTransferId(), message.contentId, node.toULong(),
            cursorVersion?.toULong(), cursorOffset?.toUInt())
    }

    override val history: TransferHistory get() = SessionTransferHistory(owner)
    override fun takeAutoOpen(): TransferOpen? = owner.takeTransferAutoOpen()?.let {
        TransferOpen(it.recordId, it.path, it.displayName, it.mime)
    }
    override fun cancelAutoOpen() { owner.cancelTransferAutoOpen() }

    override fun audioSettings(): AudioSettings {
        val media = owner.media()
        return AudioSettings(AudioTimeout.of(media.idleTimeout(MobileMediaKind.MICROPHONE)),
            AudioTimeout.of(media.idleTimeout(MobileMediaKind.SPEAKER)), media.echoCancellation())
    }
    override fun setAudioIdleTimeout(kind: MobileMediaKind, value: AudioTimeout) { owner.media().setIdleTimeout(kind, value.core) }
    override fun setEchoCancellation(enabled: Boolean) { owner.media().setEchoCancellation(enabled) }
    override fun microphoneGain(): MicrophoneGain = owner.media().microphoneControls().toGain()
    override fun setMicrophoneGain(volume: Int, muted: Boolean): MicrophoneGain =
        owner.media().setMicrophoneControls(volume.coerceIn(0, 200).toUShort(), muted).toGain()

    private fun newTransferId(): ByteArray = ByteArray(16).also(random::nextBytes)
}

private fun ByteArray.hexKey(): String = joinToString("") { "%02x".format(it) }

/** The entry as the core takes it back (its `child` snapshot is ignored there). */
private fun DirectoryEntry.toCore() = MobileDirectoryEntry(id.toULong(), parent?.toULong(), name, kind, size?.toULong(), version.toULong(), child = null)

private fun LocalDeviceName.toName() = DeviceName(name, systemName, custom)
private fun MobileMicrophoneControls.toGain() = MicrophoneGain(volumePercent.toInt(), muted, revision.toLong())

internal fun PairProblem.toFailure() = PairFailure(code, stage, PairFailureReason.of(code))

internal fun PairView.toSession() = PairSession(id, revision.toString(),
    (4 - attemptsLeft.toInt()).toUByte(), direction == Direction.INCOMING && phase == 2u,
    when {
        feedback == Feedback.WRONG -> "验证码错误，还可尝试 $attemptsLeft 次"
        phase == 0u -> "正在连接对方…"
        phase >= 3u -> "正在验证并合并设备组…"
        direction == Direction.OUTGOING -> "等待对方接受并输入验证码"
        else -> "请输入对方显示的六位码"
    },
    outgoing = direction == Direction.OUTGOING, phase = PairPhase.of(phase), attemptsLeft = attemptsLeft.toInt(),
    wrongCode = feedback == Feedback.WRONG, peer = PairPeer(peer.device, peer.name, peer.os.lowercase()))

/**
 * Every image mime type (image/png, image/jpeg, image/heic, ...) goes as an Image with its real mime;
 * the core decides direct vs. proxy. Images carry their pixel size as shown (orientation applied):
 * own pictures are drawn at that aspect (mobile-core.md 「传输页文件卡片」 1).
 */
internal fun shareItems(draft: ShareDraft, imageSize: (String) -> Pair<Int, Int>? = ::imagePixelSize,
                        isDirectory: (String) -> Boolean): List<MobileShareItem> = buildList {
    // The text with its rich text (user 2026-11-12: every format the phone has).
    if (draft.text.isNotEmpty()) add(MobileShareItem.Text(draft.text, draft.html?.takeIf { it.isNotEmpty() }))
    draft.items.forEach { item ->
        when {
            isDirectory(item.path) -> add(MobileShareItem.Folder(item.path, item.name))
            // Draft files are ShareStaging copies (cacheDir/outgoing, same volume as the core's data):
            // the core moves them into its store instead of copying them again.
            item.mime.startsWith("image/") -> {
                val pixels = imageSize(item.path)
                add(MobileShareItem.Image(item.path, item.size.toULong(), item.mime, true,
                    width = pixels?.first?.toUInt(), height = pixels?.second?.toUInt()))
            }
            else -> add(MobileShareItem.File(item.path, item.name, item.size.toULong(), item.mime.takeIf { it.isNotBlank() }, true))
        }
    }
}

/**
 * A picture's width and height as shown: the header's size from a bounds-only decode (no pixels),
 * swapped when the EXIF / HEIF orientation turns it a quarter (5–8: a portrait photo stored
 * landscape comes out portrait). Null when the platform cannot read the file as a picture.
 */
internal fun imagePixelSize(path: String): Pair<Int, Int>? {
    val bounds = android.graphics.BitmapFactory.Options().apply { inJustDecodeBounds = true }
    runCatching { android.graphics.BitmapFactory.decodeFile(path, bounds) }
    if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
    val orientation = runCatching {
        android.media.ExifInterface(path).getAttributeInt(android.media.ExifInterface.TAG_ORIENTATION, android.media.ExifInterface.ORIENTATION_NORMAL)
    }.getOrDefault(android.media.ExifInterface.ORIENTATION_NORMAL)
    return if (orientation in quarterTurns) bounds.outHeight to bounds.outWidth else bounds.outWidth to bounds.outHeight
}

/** EXIF orientations that swap width and height: transpose, rotate 90, transverse, rotate 270. */
private val quarterTurns = setOf(android.media.ExifInterface.ORIENTATION_TRANSPOSE, android.media.ExifInterface.ORIENTATION_ROTATE_90,
    android.media.ExifInterface.ORIENTATION_TRANSVERSE, android.media.ExifInterface.ORIENTATION_ROTATE_270)

internal fun controlFailureText(reason: DeviceControlFailure): String = when (reason) {
    DeviceControlFailure.OFFLINE -> "设备不在线"
    DeviceControlFailure.UNSUPPORTED, DeviceControlFailure.UNSUPPORTED_OPTION -> "对方的 Oink 版本不支持此设置"
    DeviceControlFailure.STALE_BOOT, DeviceControlFailure.STALE_REVISION -> "对方的状态已变化，请重试"
    DeviceControlFailure.NOT_MEMBER -> "该设备不在设备组中"
    DeviceControlFailure.RESOURCE_FAULT -> "对方设备出现故障"
    DeviceControlFailure.INVALID_VALUE -> "设置值无效"
    DeviceControlFailure.BUSY -> "对方正忙，请稍后再试"
    DeviceControlFailure.PERSISTENCE_FAILED -> "对方无法保存设置"
    DeviceControlFailure.APPLY_FAILED -> "对方无法应用设置"
    DeviceControlFailure.LINK_LOST -> "连接已断开"
    DeviceControlFailure.OUTCOME_UNKNOWN -> "不确定是否已生效，请检查对方状态"
    DeviceControlFailure.NOT_EXIT -> "该设备不是音频出口"
    DeviceControlFailure.ROUTE_CHANGED -> "音频线路已变化，请重试"
    DeviceControlFailure.OUTPUT_CHANGED -> "音频输出设备已变化，请重试"
    DeviceControlFailure.OUTPUT_UNAVAILABLE -> "对方没有可用的音频输出"
    DeviceControlFailure.VOLUME_UNSUPPORTED -> "对方不支持调节音量"
    DeviceControlFailure.PERMISSION_DENIED -> "对方没有授予所需权限"
    DeviceControlFailure.SUPERSEDED -> "已被新的设置取代"
    DeviceControlFailure.STATE_CONFLICT -> "对方的状态有冲突，请重试"
    DeviceControlFailure.UNRECOGNIZED -> "未知原因"
}
