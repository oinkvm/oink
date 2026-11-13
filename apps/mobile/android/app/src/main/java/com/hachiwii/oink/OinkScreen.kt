package com.hachiwii.oink

import android.content.ContentValues
import android.content.Intent
import android.provider.MediaStore
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.runtime.*
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.hachiwii.oink.recovery.PairingCode
import com.hachiwii.oink.recovery.PairingCodeNotice
import com.hachiwii.oink.recovery.PairingCodeState
import com.hachiwii.oink.recovery.RecoveryController
import com.hachiwii.oink.recovery.RecoveryPhase
import com.hachiwii.oink.recovery.RecoveryReason
import com.hachiwii.oink.ui.*
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.conflate
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.oink_mobile_ffi.CardActionUnavailable
import uniffi.oink_mobile_ffi.CardCondition
import uniffi.oink_mobile_ffi.GameModePreference
import uniffi.oink_mobile_ffi.MobileImagePreview
import uniffi.oink_mobile_ffi.MicrophoneMode
import uniffi.oink_mobile_ffi.MobileDeliveryFailure
import uniffi.oink_mobile_ffi.MobileDeliveryState
import uniffi.oink_mobile_ffi.MobileDownloadStatus
import uniffi.oink_mobile_ffi.MobileEndpointState
import uniffi.oink_mobile_ffi.ResourceMode
import uniffi.oink_mobile_ffi.SpeakerActivity
import java.net.NetworkInterface
import java.util.Locale

/** A design scene: presentation models built by a debug-only fixture (see src/debug). */
class DesignScene(val model: AppModel, val init: UiInit, val prefs: Prefs, val dark: Boolean?)

/** Debug builds install the design fixtures here; release builds leave it empty. */
object DesignHooks { @Volatile var fixture: ((String) -> DesignScene?)? = null }

/** One volume change; changes queue up latest-wins while one is being sent. */
private data class LevelRequest(val device: String, val type: String, val level: Int)

/** Root of the Oink UI: maps the repository into presentation models and actions. */
@Composable
fun OinkScreen(repository: OinkRepository, share: Boolean, scene: DesignScene?,
               onAppearanceChanged: (String, String) -> Unit) {
    val systemDark = isSystemInDarkTheme()
    if (scene != null) {
        var prefs by remember(scene) { mutableStateOf(scene.prefs) }
        OinkApp(scene.model, scene.init, prefs, { prefs = it }, AppActions(), scene.dark ?: systemDark)
        return
    }
    val state by repository.state.collectAsStateWithLifecycle()
    val feedbackView = androidx.compose.ui.platform.LocalView.current
    LaunchedEffect(repository, feedbackView) {
        repository.speakerHaptics.collect { success -> feedbackView.performHapticFeedback(
            if (success) android.view.HapticFeedbackConstants.CONFIRM else android.view.HapticFeedbackConstants.REJECT) }
    }
    val context = LocalContext.current
    val settings = remember(context) { LocalSettings(context) }
    val scope = rememberCoroutineScope()
    val snapshot = state.snapshot

    // ---- Privileged mode (RecoveryController owns it; "setup" is the UI's own wizard step) ----
    val recovery by RecoveryController.status.collectAsStateWithLifecycle()
    var privilegedUi by remember { mutableStateOf<String?>(null) }
    var privilegedFailure by remember { mutableStateOf<String?>(null) }
    val privileged = privilegedUi ?: privilegedFromRecovery(recovery.phase, recovery.detail, RecoveryController.enabled(context))
    val pointerProblem by RecoveryController.pointerProblem.collectAsStateWithLifecycle()
    LaunchedEffect(recovery.phase) { if (privilegedUi == "starting" && recovery.phase != RecoveryPhase.Pending) privilegedUi = null }
    // 第 3 步's pairing, from the app's field or from the pairing notification: the page follows it.
    val codePairing by PairingCode.of(context).state.collectAsStateWithLifecycle()
    LaunchedEffect(codePairing) {
        (codePairing as? PairingCodeState.Failed)?.let { privilegedFailure = it.message }
        privilegedUi = privilegedOverrideAfter(codePairing, privilegedUi)
    }
    val recoveryFailure = when (recovery.reason) {
        RecoveryReason.PermissionDenied -> "系统没有允许 Oink 使用无线调试。"
        RecoveryReason.Expired -> "无线调试的配对已失效，请重新配对。"
        RecoveryReason.StartFailed -> "特权服务没能启动，请重试。"
        RecoveryReason.Unavailable -> "无线调试不可用。请确认手机已连接 Wi‑Fi，并且「无线调试」已打开。"
        else -> "无法连接无线调试。请确认手机已连接 Wi‑Fi，并且「无线调试」已打开。"
    }

    // ---- Settings: local ones live in LocalSettings, core-owned ones are read from the state ----
    var prefs by remember { mutableStateOf(loadPrefs(settings)) }
    val shownPrefs = prefs.copy(
        privileged = privileged,
        cacheBytes = state.cacheBytes ?: 0,
        micAutoOff = state.audioSettings?.microphoneTimeout?.minutes ?: prefs.micAutoOff,
        speakerAutoOff = state.audioSettings?.speakerTimeout?.minutes ?: prefs.speakerAutoOff,
        echoCancel = state.audioSettings?.echoCancellation ?: prefs.echoCancel,
        audioAvailable = state.audioSettings != null,
        persistentNotice = state.persistentNotification,
    )
    // A phone-to-phone notification opens the transfer page at its message.
    val focus = (context as? MainActivity)?.focusMessage
    // The persistent notification (「打开 App」) or a tile asked for a page.
    val openTab = (context as? MainActivity)?.openTab
    val init = remember(focus, openTab) {
        val tab = when {
            share || focus != null -> Tab.Transfer
            openTab != null -> Tab.entries[openTab.tab.ordinal]
            else -> prefs.homeTab
        }
        UiInit(tab = tab, transferProxy = settings.transferTarget, transferFocus = focus)
    }
    val setPrefs: (Prefs) -> Unit = { next ->
        val old = shownPrefs
        if (next.privileged != old.privileged) { privilegedUi = next.privileged; privilegedFailure = null }
        if (next.accent != old.accent || next.scheme != old.scheme) {
            settings.themeColor = next.accent.id; settings.themeMode = next.scheme
            onAppearanceChanged(next.scheme, next.accent.id)
        }
        if (next.language != old.language) settings.language = next.language
        if (next.homeTab != old.homeTab) settings.homeTab = HomeTab.entries[next.homeTab.ordinal]
        if (next.autoWriteClipboard != old.autoWriteClipboard) repository.setAutomatic(next.autoWriteClipboard)
        if (next.autoReadClipboard != old.autoReadClipboard) {
            settings.autoReadClipboard = next.autoReadClipboard; repository.setPrivilegedAutoRead(next.autoReadClipboard)
            if (next.autoReadClipboard) repository.readNativeAndBroadcast()
        }
        if (next.speakerTap != old.speakerTap) settings.speakerTap = next.speakerTap
        if (next.micAutoOff != old.micAutoOff) repository.setAudioIdleTimeout(uniffi.oink_mobile_ffi.MobileMediaKind.MICROPHONE, AudioTimeout.fromMinutes(next.micAutoOff))
        if (next.speakerAutoOff != old.speakerAutoOff) repository.setAudioIdleTimeout(uniffi.oink_mobile_ffi.MobileMediaKind.SPEAKER, AudioTimeout.fromMinutes(next.speakerAutoOff))
        if (next.echoCancel != old.echoCancel) repository.setEchoCancellation(next.echoCancel)
        if (next.persistentNotice != old.persistentNotice) repository.setPersistentNotification(next.persistentNotice)
        if (next.noticeTap != old.noticeTap) repository.setNoticeTap(if (next.noticeTap == "open") NoticeTap.OPEN else NoticeTap.COPY)
        if (next.noticeTab != old.noticeTab) repository.setNoticeTab(HomeTab.entries[next.noticeTab.ordinal])
        if (next.hideRecents != old.hideRecents) settings.hideInRecents = next.hideRecents
        if (next.openWith != old.openWith) settings.openFiles = next.openWith
        if (next.openWithGroups != old.openWithGroups) OpenFileGroup.entries.forEach { g ->
            val v = next.openWithGroups[groupKey(g)] == true
            if (v != (old.openWithGroups[groupKey(g)] == true)) settings.select(g, v)
        }
        if (next.autoRecover != old.autoRecover) settings.autoRecover = next.autoRecover
        // Only the locally stored values are kept here; the core-owned ones come back through the state.
        prefs = next.copy(privileged = prefs.privileged, cacheBytes = prefs.cacheBytes,
            micAutoOff = prefs.micAutoOff, speakerAutoOff = prefs.speakerAutoOff, echoCancel = prefs.echoCancel,
            persistentNotice = prefs.persistentNotice)
    }

    // ---- Pairing: the sheet follows the core's session; the result comes as a one-shot notice ----
    var pairingPeer by remember { mutableStateOf<NearbyUi?>(null) }
    var lastPairing by remember { mutableStateOf<PairingView?>(null) }
    // The code this device submitted: the cells keep it while it is verified and on success.
    var typedCode by remember { mutableStateOf("") }
    val toasts = remember { MutableSharedFlow<String>(extraBufferCapacity = 8) }
    val livePairing = sessionView(snapshot, pairingPeer, lastPairing, typedCode)
    LaunchedEffect(livePairing) { if (livePairing != null) lastPairing = livePairing }
    val notice = state.pairingNotice
    val pairingSuccess = rememberCompletedPairing(notice,
        takeShown = { lastPairing.also { lastPairing = null; pairingPeer = null } },
        clearNotice = repository::clearPairingNotice, toast = { toasts.tryEmit(it) })
    LaunchedEffect(notice) {
        if (notice is PairingNotice.Failed && lastPairing == null) {
            // No sheet was open for it (the screen was recreated meanwhile). A request that
            // ended before it was accepted is never reported: it only leaves the list.
            repository.clearPairingNotice()
            toasts.tryEmit("配对失败：${pairProblemText(notice.failure.code)}")
        }
    }
    val failedPairing = (notice as? PairingNotice.Failed)?.let { failed ->
        lastPairing?.copy(status = "failed", reason = pairProblemText(failed.failure.code))
    }
    val pairing = pairingSuccess ?: livePairing ?: failedPairing

    // ---- Volume changes: latest wins while one is in flight ----
    val levels = remember { MutableSharedFlow<LevelRequest>(extraBufferCapacity = 64) }
    LaunchedEffect(Unit) {
        levels.conflate().collect { request ->
            val current = repository.state.value
            val device = current.snapshot.devices.firstOrNull { it.id == request.device } ?: return@collect
            // A phone exit has no desktop controls; its bar is the exit volume.
            if (device.controls == null && request.type != "mic" && current.snapshot.exit?.deviceId == device.id) {
                repository.setExitVolume(request.level, current.snapshot.exit?.muted ?: false)?.join()
                return@collect
            }
            val controls = device.controls ?: return@collect
            // setExitVolume reports its own failure; the forwarding controls are checked here.
            val job = when {
                request.type == "mic" -> controls.microphone.volume?.let { repository.setForwardingVolume(it, request.level, it.muted) }
                    ?: run { repository.fail("此设备的麦克风音量暂不可调"); null }
                controls.speaker.isExit -> repository.setExitVolume(request.level, current.snapshot.exit?.muted ?: false)
                else -> controls.speaker.forwardingVolume?.let { repository.setForwardingVolume(it, request.level, it.muted) }
                    ?: run { repository.fail("此设备的扬声器音量暂不可调"); null }
            }
            job?.join()
        }
    }

    val selfDevice = snapshot.devices.firstOrNull { it.isSelf }
    val devices = snapshot.devices
    val peers = devices.filter { !it.isSelf }.map { it.toPeer(snapshot.exit) }
    val audio = state.localAudio
    val pairingPort by PairingServiceFinder.port.collectAsStateWithLifecycle()
    val debugSwitches = rememberDebugSwitches()
    val lookup = snapshot.addressLookup
    var manualInput by remember { mutableStateOf("" to 47470) }
    // A system share shows only the operation its own ShareActivity started (review M39).
    val shareId = (context as? ShareActivity)?.shareId
    val shareState = shareId?.let { state.shares[it] }
    val transferPeers = devices.associateBy { it.id }
    // Where the transfer page / share picker can send (core TransferTargetsView); nothing yet while
    // the session starts.
    val targets = state.targets?.toUi(transferPeers) ?: TransferTargetsUi(emptyList(), 0, searching = true)
    // Rich text of text messages (user 2026-11-12): the core's cleaned markup, read off the main thread once per
    // version of a message (its rich text arrives after the text and the message is announced again then).
    val richHtml = remember { mutableStateMapOf<Long, String>() }
    val richSeen = remember { HashMap<Long, TransferMessage>() }
    val previousMessages = remember { HashMap<Long, TransferMessage>() }
    LaunchedEffect(state.transferMessages) {
        val live = state.transferMessages.mapTo(HashSet()) { it.id }
        if (previousMessages.values.any { it.sensitive && it.id !in live }) ImageCache.evictAll()
        previousMessages.clear()
        state.transferMessages.forEach { previousMessages[it.id] = it }
        richSeen.keys.retainAll(live)
        richHtml.keys.retainAll(live)
        val todo = state.transferMessages.filter { it.kind.equals("text", true) && it.richTextFormat != null && richSeen[it.id] != it }
        if (todo.isEmpty()) return@LaunchedEffect
        val found = kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
            todo.map { it to repository.richText(it)?.displayHtml?.takeIf { html -> html.isNotBlank() } }
        }
        found.forEach { (m, html) -> richSeen[m.id] = m; if (html != null) richHtml[m.id] = html else richHtml.remove(m.id) }
    }
    val model = AppModel(
        selfName = state.name,
        peers = peers,
        targets = targets,
        requests = snapshot.requests.map { r ->
            NearbyUi(r.id, r.name, platformOf(r.platform), "", 0, fingerprintOf(r.deviceId), groupSize = r.sourceCount)
        },
        nearby = snapshot.nearby.map { it.toNearby() },
        self = SelfInfo(localAddresses(), state.selfPort, selfDevice?.fingerprint.orEmpty()),
        messages = state.transferMessages.map { t -> t.toUi(state, transferPeers).let { u -> richHtml[t.id]?.let { u.copy(richHtml = it) } ?: u } },
        composer = state.draft.text,
        share = (shareState ?: ShareUiState()).toCard(share),
        pairing = pairing,
        manual = ManualState(
            searching = lookup is AddressLookup.Searching,
            found = (lookup as? AddressLookup.Found)?.device?.let { d ->
                NearbyUi(d.id, d.name, platformOf(d.platform), manualInput.first, manualInput.second, d.fingerprint)
            },
            error = (lookup as? AddressLookup.Failed)?.let { addressProblemText(it.network, it.failure.code) },
        ),
        speakerOn = audio.speakerOn, micOn = audio.microphoneOn, forwardOn = audio.forwardOn,
        forwardFailed = audio.forwardFailure != uniffi.oink_mobile_ffi.MobileMediaFailure.NONE,
        speakerFailed = audio.speakerOn && audio.speaker == MobileEndpointState.FAILED,
        micFailed = audio.microphoneOn && audio.microphone == MobileEndpointState.FAILED,
        version = remember(context) { AppVersion.label(context) },
        pairingPort = pairingPort,
        developerOptionsOn = debugSwitches.developerOptions,
        wirelessDebuggingOn = debugSwitches.wirelessDebugging,
        deviceName = state.deviceName?.let { DeviceNameUi(it.name, it.systemName, it.custom) },
        worksInBackground = state.worksInBackground,
        rebootNeedsWirelessDebugging = state.privilegedRebootNeedsWirelessDebugging,
        hasOlderMessages = state.transferNextCreatedMs != null || state.transferSharedMore,
        loadingOlder = state.transferLoading, olderLoadFailed = state.transferLoadFailed, sharedBoundaryMs = state.transferSharedBoundary,
        folders = folderModels(state),
        privilegedFailure = privilegedFailure ?: recoveryFailure,
        privilegedPointerProblem = pointerProblem,
        connectionFailure = state.startupFailure,
    )

    // Picked attachments are their own send to the chosen proxy, never part of the composer draft;
    // the repository keeps the proxy and the connection while the system picker is open (H18, M36).
    val photos = rememberLauncherForActivityResult(ActivityResultContracts.PickMultipleVisualMedia()) { uris ->
        repository.finishPick(if (uris.isEmpty()) null else suspend { ShareStaging.copy(context, uris) })
    }
    val files = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        repository.finishPick(if (uris.isEmpty()) null else suspend { ShareStaging.copy(context, uris) })
    }
    // 「文件夹」: the picked tree is copied into staging as a directory, which the core sends as a folder.
    val folderPicker = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        repository.finishPick(if (uri == null) null else suspend { ShareStaging.folder(context, uri) })
    }
    val camera = rememberLauncherForActivityResult(ActivityResultContracts.TakePicturePreview()) { bitmap ->
        repository.finishPick(if (bitmap == null) null else suspend { ShareStaging.bitmap(context, bitmap) })
    }
    fun pick(target: String?, launch: () -> Unit) {
        repository.beginPick(target)
        try { launch() }
        catch (_: android.content.ActivityNotFoundException) { repository.finishPick(null); repository.fail("没有可用的应用") }
    }
    fun device(peer: PeerUi) = repository.state.value.snapshot.devices.firstOrNull { it.id == peer.id }
    fun message(id: Long) = repository.state.value.transferMessages.firstOrNull { it.id == id }
    fun folderEntry(folder: FolderUi, item: FolderItemUi) =
        repository.state.value.folderPages[FolderPageKey(folder.messageId, folder.node)]?.entries?.firstOrNull { it.id == item.entryId }
    // Stable while the core runs (the control page keeps its session); a new core gives a new opener,
    // so the page reopens its session on the new one.
    val controlOpener = remember(repository, state.connected) {
        if (state.connected) com.hachiwii.oink.control.ControlOpener { repository.openControl(it) } else null
    }
    val actions = AppActions(
        deviceInput = { peer, type, mode ->
            val controls = device(peer)?.controls
            val resource = if (type == "mouse") controls?.mouse else controls?.keyboard
            if (resource == null) repository.fail("还没有收到 ${peer.name} 的状态，请稍候")
            else repository.invokeDeviceAction(when (mode) {
                "shared" -> resource.shared
                "local" -> resource.local
                else -> resource.disabled ?: ControlAction(null, unavailable = CardActionUnavailable.UNSUPPORTED)
            })
        },
        devicePreference = { peer, key, mode ->
            val d = device(peer)
            val c = d?.controls
            when {
                key == "paused" -> d?.pause?.let { repository.invokeDeviceAction(it) } ?: repository.fail("此设备暂时无法暂停")
                c == null -> repository.fail("还没有收到 ${peer.name} 的状态，请稍候")
                key == "gaming" -> repository.invokeDeviceAction(when (mode) { "on" -> c.game.on; "off" -> c.game.off; else -> c.game.auto })
                key == "locked" -> when {
                    mode == "on" && !c.focus.locked ->
                        // Locking the pointer turns game mode off (design: control-page.md 2.3).
                        repository.invokeDeviceActions(listOf(c.focus.toggle) + if (c.game.preference != GameModePreference.OFF) listOf(c.game.off) else emptyList())
                    mode == "off" && c.focus.locked -> repository.invokeDeviceAction(c.focus.toggle)
                    else -> {}
                }
                key == "speaker" -> {
                    val sp = c.speaker
                    val forwarding = peer.speaker == "forward"
                    repository.invokeDeviceActions(when (mode) {
                        "outlet" -> if (sp.isExit) emptyList() else listOf(sp.exitToggle)
                        "forward" -> (if (sp.isExit) listOf(sp.exitToggle) else emptyList()) + if (forwarding) emptyList() else listOf(sp.forwardingOn)
                        else -> (if (sp.isExit) listOf(sp.exitToggle) else emptyList()) + if (forwarding) listOf(sp.forwardingOff) else emptyList()
                    })
                }
                // As the PC control plan: a sending device first releases its share claim (its
                // local action); release alone already lets it receive. Flipping "receive" while it
                // still sends changes nothing (receive is not muted), which looked like a no-op.
                key == "mic" -> repository.invokeDeviceAction(when {
                    mode == "send" -> c.microphone.share
                    c.microphone.mode == MicrophoneMode.SOURCE -> c.microphone.local
                    mode == "receive" -> c.microphone.receive
                    else -> c.microphone.local
                })
            }
        },
        deviceLevel = { peer, type, level -> levels.tryEmit(LevelRequest(peer.id, type, level)) },
        kick = { peer -> repository.removeMember(peer.id) },
        memberAddress = { peer -> runCatching { repository.memberAddresses(peer.id).firstOrNull() }.getOrNull() },
        editIp = { peer, ip, port ->
            val address = if (port.isEmpty()) ip else if (ip.contains(':')) "[$ip]:$port" else "$ip:$port"
            runCatching { repository.setMemberAddress(peer.id, address) }.fold(
                onSuccess = { result -> if (result == EditResult.Ok) null else editMessage(result) },
                onFailure = { it.oinkMessage ?: "无法修改 IP" })
        },
        refreshNearby = { repository.refreshNearby() },
        pair = { peer ->
            val target = snapshot.nearby.firstOrNull { it.id == peer.id }?.pairTarget
                ?: (lookup as? AddressLookup.Found)?.device?.takeIf { it.id == peer.id }?.pairTarget
            if (target == null) repository.fail("无法与 ${peer.name} 配对")
            else { pairingPeer = peer; lastPairing = null; typedCode = ""; repository.clearPairingNotice(); repository.command { it.pair(target) } }
        },
        accept = { req ->
            snapshot.requests.firstOrNull { it.id == req.id }?.let { r ->
                pairingPeer = req; lastPairing = null; typedCode = ""; repository.clearPairingNotice(); repository.command { it.accept(r) }
            }
        },
        reject = { req -> snapshot.requests.firstOrNull { it.id == req.id }?.let { r -> repository.command { it.reject(r) } } },
        submitCode = { code -> snapshot.pairingSession?.let { s -> typedCode = code; repository.command { it.submitCode(s, code) } } },
        cancelPairing = {
            // Neither "关闭" after a failure nor "取消配对" shows a toast (user 2026-11-11, same as iOS).
            if (failedPairing != null && livePairing == null) {
                repository.clearPairingNotice(); lastPairing = null; pairingPeer = null
            } else repository.command { if (it.cancelPairing()) { lastPairing = null; pairingPeer = null } }
        },
        manualSearch = { ip, port -> manualInput = ip to (port ?: 47470); repository.command { it.queryAddress(ip, port) } },
        manualReset = { repository.command { it.releaseAddress() } },
        composer = { repository.setDraft(state.draft.copy(text = it)) },
        sendText = { target -> repository.send(target) },
        pickImages = { target -> pick(target) { photos.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageAndVideo)) } },
        pickFiles = { target -> pick(target) { files.launch(arrayOf("*/*")) } },
        takePhoto = { target -> pick(target) { camera.launch(null) } },
        pickFolder = { target -> pick(target) { folderPicker.launch(null) } },
        download = { m -> message(m.id)?.let { repository.downloadTransfer(it, false) } },
        openFile = { m -> message(m.id)?.let { repository.downloadTransfer(it, true) } },
        requestThumbnail = { m -> message(m.id)?.let { repository.requestThumbnail(it) } },
        saveImage = { m ->
            val path = m.imagePath ?: m.localPath
            if (path == null) repository.fail("还没有下载完成")
            else scope.launch { repository.notice(saveToGallery(context, path, m.name, m.mime ?: message(m.id)?.mime, video = viewerKind(m) == ViewerKind.Video)) }
        },
        shareFile = { m ->
            val path = m.imagePath ?: m.localPath
            if (path == null) repository.fail("还没有下载完成")
            else runCatching { shareLocalFile(context, java.io.File(path), m.mime) }.onFailure { repository.fail("无法分享：${it.oinkMessage ?: "未知错误"}") }
        },
        openWith = { m ->
            val path = m.imagePath ?: m.localPath
            if (path == null) repository.fail("还没有下载完成")
            else runCatching { viewLocalFile(context, java.io.File(path), m.mime) }.onFailure {
                repository.fail(if (it is android.content.ActivityNotFoundException) "没有可以打开 ${m.name} 的应用" else "无法打开：${it.oinkMessage ?: "未知错误"}")
            }
        },
        openFolder = { m -> message(m.id)?.let { repository.listDirectory(it); repository.loadFolderChildren(it.id) } },
        folderEnter = { folder, item -> message(folder.messageId)?.let { repository.listDirectory(it, item.entryId) } },
        folderMore = { folder -> message(folder.messageId)?.let { repository.loadMoreDirectory(it, folder.node) } },
        folderRetry = { folder -> message(folder.messageId)?.let { repository.retryDirectory(it, folder.node) } },
        folderDownload = { folder, item, open ->
            val m = message(folder.messageId); val entry = folderEntry(folder, item)
            if (m != null && entry != null) repository.downloadFolderFile(m, entry, open)
        },
        folderThumbnail = { folder, item ->
            val m = message(folder.messageId); val entry = folderEntry(folder, item)
            if (m != null && entry != null) repository.requestFolderThumbnail(m, entry)
        },
        copyMessage = { m -> message(m.id)?.let { repository.copyTransfer(it) } ?: false },
        copyMessageText = { m -> message(m.id)?.let { repository.copyTransferText(it) } ?: false },
        cancelTransfer = { m -> message(m.id)?.let { repository.cancelTransfer(it) } },
        chooseTarget = { id -> settings.transferTarget = id },
        deleteMessage = { m -> message(m.id)?.let { repository.deleteTransfer(it) } },
        loadOlder = { repository.loadTransfers() },
        leaveTransfer = { if (repository.state.value.openingRecord != null) repository.cancelAutoOpen() },
        clearCache = { ShareStaging.clearAbandoned(context); repository.clearTransferCacheNow() },
        leaveGroup = { repository.leaveGroup() },
        clearHistory = { repository.clearTransferHistoryNow() },
        refreshCache = { repository.refreshCacheBytes() },
        privilegeStart = { code ->
            privilegedFailure = null
            // The pairing runs in the process scope (the notification's replies use the same one): it is not
            // cut off when this Activity goes away. Its result reaches the page through [PairingCode]'s state.
            if (code.length == 6) PairingCodeNotice.submit(context, code, viaNotice = false)
            else scope.launch {
                PairingCode.of(context).reset()
                runCatching { RecoveryController.setEnabled(context, true); RecoveryController.start(context) }
                    .onFailure { privilegedFailure = it.oinkMessage ?: recoveryFailure; privilegedUi = "failed" }
            }
        },
        privilegeDisable = { RecoveryController.setEnabled(context, false) },
        privilegeReset = { runCatching { RecoveryController.resetCredentials(context) }.onFailure { repository.fail(it.oinkMessage ?: "无法重置配对凭据") } },
        openSystem = { which ->
            // Some ROMs have no activity for the developer-options action (or refuse it while the
            // options are off): the main Settings page is the next best start, Developer options is
            // one level under System there.
            val primary = if (which == "about") Settings.ACTION_DEVICE_INFO_SETTINGS else Settings.ACTION_APPLICATION_DEVELOPMENT_SETTINGS
            val opened = listOf(primary, Settings.ACTION_SETTINGS).any { action ->
                runCatching { context.startActivity(Intent(action).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)) }.isSuccess
            }
            if (!opened) repository.fail("无法打开系统设置")
        },
        pairingService = { on -> PairingCodeNotice.setStepActive(context, on) },
        toggleSpeaker = { repository.speakerPressed() },
        longPressSpeaker = { repository.speakerPressed(long = true) },
        receiveSpeaker = repository::toggleSpeaker,
        toggleMic = { repository.toggleMicrophone() },
        setDeviceName = { repository.setLocalDeviceName(it) },
        retryConnection = { repository.retryConnection() },
        control = controlOpener,
    )
    if (share && shareId == null) {
        // System share entry, first step: pick the device (native-rewrite.md 「分享 / 打开方式先选设备」).
        val activity = context as? ShareActivity
        val shownPrefsNow = shownPrefs
        ShareTheme(shownPrefsNow, systemDark) {
            SharePickerFlow(targets, state.connected, repository, activity?.summary.orEmpty(), activity?.knownBytes, activity?.routingReady == true, onPick = { peer, only -> activity?.pick(peer, only) }, onDismiss = { activity?.finish() })
        }
        return
    }
    if (share) {
        // System share entry: only the extension card over the sharing app. The repository sends this
        // Activity's own operation (the first online computer, or every online device for text and
        // small images) and waits while devices are still connecting; the card closes 0.8 s after
        // delivery. An Activity restored without its operation (process restarted) fails visibly.
        val activity = context as? android.app.Activity
        val phase = if (shareId != null && shareState == null && activity?.isFinishing != true) SharePhase.FAILED else shareState?.phase
        LaunchedEffect(phase) {
            when (phase) {
                SharePhase.SUCCEEDED -> { delay(800); activity?.finish() }
                SharePhase.FAILED -> {
                    android.widget.Toast.makeText(context, shareState?.status ?: "分享已中断，请重新分享", android.widget.Toast.LENGTH_LONG).show()
                    activity?.finish()
                }
                else -> {}
            }
        }
        val id = shareId
        // 隐藏 (or a swipe down): the share goes on in the background with its progress in a notification.
        ShareOnly(model.share ?: ShareCardUi("indeterminate"), shownPrefs, systemDark,
            onHide = { if (id != null) { repository.hideShare(id); activity?.finish() } },
            onCancel = { if (id != null) repository.cancelShare(id) })
        return
    }
    OinkApp(model, init, shownPrefs, setPrefs, actions, systemDark, notices = remember { kotlinx.coroutines.flow.merge(repository.notices, toasts) })
}

private fun groupKey(g: OpenFileGroup) = when (g) {
    OpenFileGroup.Images -> "image"; OpenFileGroup.Videos -> "video"; OpenFileGroup.Audio -> "audio"; OpenFileGroup.Documents -> "document"
    OpenFileGroup.Archives -> "archive"; OpenFileGroup.Packages -> "apk"; OpenFileGroup.Other -> "other"
}

private fun loadPrefs(s: LocalSettings) = Prefs(
    accent = Accent.of(s.themeColor), scheme = s.themeMode, language = s.language, homeTab = Tab.entries[s.homeTab.ordinal],
    autoWriteClipboard = s.autoWriteClipboard, autoReadClipboard = s.autoReadClipboard,
    speakerTap = s.speakerTap,
    persistentNotice = s.persistentNotification, hideRecents = s.hideInRecents, openWith = s.openFiles,
    noticeTap = if (s.noticeTap == NoticeTap.OPEN) "open" else "copy", noticeTab = Tab.entries[s.noticeTab.ordinal],
    openWithGroups = OpenFileGroup.entries.associate { groupKey(it) to s.selected(it) }, autoRecover = s.autoRecover,
)

internal fun platformOf(os: String) = when (os.lowercase(Locale.US)) {
    "windows" -> "windows"; "macos", "mac", "darwin" -> "mac"; "ios", "iphone" -> "iphone"; "ipados", "ipad" -> "ipad"; "android" -> "android"; else -> "other"
}

private fun fingerprintOf(device: String): String =
    if (device.isEmpty()) "" else runCatching { uniffi.oink_mobile_ffi.deviceFingerprint(device) }.getOrDefault("")

private fun lastSeenText(ms: Long?): String {
    val ago = ms?.let { (System.currentTimeMillis() - it).coerceAtLeast(0) / 60_000 } ?: return "很久"
    return when { ago < 60 -> "${maxOf(1, ago)} 分钟"; ago < 1440 -> "${ago / 60} 小时"; else -> "${ago / 1440} 天" }
}

private fun ResourceState.ui(): String = when {
    condition == CardCondition.FAULTED || condition == CardCondition.UNSUPPORTED -> "unavailable"
    mode == ResourceMode.SHARED -> "shared"
    mode == ResourceMode.LOCAL -> "local"
    mode == ResourceMode.DISABLED -> "blocked"
    else -> "unavailable"
}

/** One device card as the list, the sheet and the transfer page show it (one state source). */
internal fun Device.toPeer(exit: ExitState?): PeerUi {
    val base = PeerUi(id, name, platformOf(platform), offline = !online, latency = latencyMillis, lastSeen = lastSeenText(lastSeenMillis),
        incompatible = incompatible, paused = paused == true, privileged = androidPrivileged && online)
    val c = controls
    if (c == null) {
        // Phones publish receive, microphone and forward endpoints from the same media session.
        val audio = mobileAudio ?: return base.copy(mouse = "unavailable", keyboard = "unavailable", speaker = "unavailable", mic = "unavailable",
            speakerAdjustable = false, micAdjustable = false)
        val speaker = when {
            audio.forward == MobileEndpointState.ACTIVE || audio.forward == MobileEndpointState.STARTING -> "forward"
            audio.speaker == MobileEndpointState.ACTIVE || audio.speaker == MobileEndpointState.STARTING -> "outlet"
            audio.forward == MobileEndpointState.FAILED -> "unavailable"
            audio.speaker == MobileEndpointState.LOCAL -> "local"
            else -> "unavailable"
        }
        // A phone that is the group's exit shows its exit volume like a PC exit:
        // the system media volume (Android) or the iPhone's playout gain.
        val mine = exit?.takeIf { it.deviceId == id && speaker == "outlet" }
        return base.copy(
            speaker = speaker,
            mic = when (audio.microphone) { MobileEndpointState.ACTIVE, MobileEndpointState.STARTING -> "send"; MobileEndpointState.LOCAL -> "local"; else -> "unavailable" },
            speakerLevel = mine?.volume ?: 0,
            speakerAdjustable = mine?.control != null, micAdjustable = false)
    }
    val sp = c.speaker
    val speaker = when {
        sp.isExit -> "outlet"
        sp.activity == SpeakerActivity.FAILED -> "unavailable"
        // The setting decides 转发, as on the PC; activity is only what is playing right now.
        sp.forwardingEnabled == true -> "forward"
        sp.forwardingEnabled == null && sp.activity in setOf(SpeakerActivity.FORWARDING, SpeakerActivity.SOUND_SLEEP, SpeakerActivity.WAITING_FOR_EXIT, SpeakerActivity.PREPARING) -> "forward"
        sp.exitToggle.unavailable == CardActionUnavailable.UNSUPPORTED -> "unavailable"
        else -> "local"
    }
    val mine = exit?.takeIf { it.deviceId == id }
    val speakerLevel = when (speaker) {
        "outlet" -> mine?.volume
        "forward" -> sp.forwardingVolume?.volume
        else -> null
    }
    val pref = c.game.preference
    return base.copy(
        mouse = c.mouse.ui(), keyboard = c.keyboard.ui(),
        speaker = speaker,
        mic = when (c.microphone.mode) {
            // Idle = receiving is on but nobody sends right now; only Muted is 本地.
            MicrophoneMode.SOURCE -> "send"; MicrophoneMode.RECEIVER, MicrophoneMode.IDLE -> "receive"; MicrophoneMode.UNAVAILABLE -> "unavailable"; else -> "local"
        },
        speakerLevel = speakerLevel ?: 0, micLevel = c.microphone.volume?.volume ?: 0,
        speakerAdjustable = if (speaker == "outlet") mine?.control != null else sp.forwardingVolume != null,
        micAdjustable = c.microphone.volume != null,
        locked = c.focus.locked,
        gaming = when (pref) { GameModePreference.AUTO -> "auto"; GameModePreference.ON -> "on"; else -> "off" },
        gamingActive = c.game.active ?: (pref == GameModePreference.ON),
    )
}

internal fun Device.toNearby(): NearbyUi = NearbyUi(id, name, platformOf(platform), address?.address.orEmpty(), address?.port ?: 47470,
    deviceId = fingerprint, incompatible = incompatible || pairTarget == null, groupSize = groupSize ?: 1)

/**
 * A completed pairing keeps its sheet on "配对成功" for [PAIRING_SUCCESS_MS], then closes it and toasts.
 * The hold runs in an effect keyed on its own state: clearing the notice recomposes with a new
 * notice key, which would cancel a delay running inside the notice's effect and strand the sheet.
 */
@Composable
internal fun rememberCompletedPairing(notice: PairingNotice?, takeShown: () -> PairingView?, clearNotice: () -> Unit, toast: (String) -> Unit): PairingView? {
    var success by remember { mutableStateOf<Pair<PairingView?, String>?>(null) }
    LaunchedEffect(notice) {
        if (notice is PairingNotice.Completed) {
            success = takeShown()?.copy(status = "success") to "已与 ${notice.peer.name} 配对"
            clearNotice()
        }
    }
    LaunchedEffect(success) {
        val (shown, text) = success ?: return@LaunchedEffect
        if (shown != null) delay(PAIRING_SUCCESS_MS)
        success = null
        toast(text)
    }
    return success?.first
}

internal const val PAIRING_SUCCESS_MS = 900L

/**
 * The core's pairing session as the sheet shows it; [tapped] supplies the address of a nearby row.
 * [typed] is the code this device submitted: the entry cells keep it while it is verified and on
 * success (a wrong code clears them for the next try).
 */
internal fun sessionView(s: CoreSnapshot, tapped: NearbyUi?, last: PairingView?, typed: String = ""): PairingView? {
    val session = s.pairingSession ?: return null
    val peer = session.peer
    val who = when {
        peer == null -> tapped ?: NearbyUi("", "对方设备", "other", "", 47470)
        tapped != null && (tapped.id == peer.deviceId || tapped.name == peer.name) -> tapped.copy(name = peer.name, platform = platformOf(peer.platform),
            deviceId = tapped.deviceId.ifEmpty { fingerprintOf(peer.deviceId) })
        else -> NearbyUi(peer.deviceId, peer.name, platformOf(peer.platform), "", 47470, fingerprintOf(peer.deviceId))
    }
    val dir = if (session.outgoing) "out" else "in"
    // The outgoing code stays on screen while the peer verifies it.
    val code = s.pairingCode ?: last?.code?.takeIf { last.dir == "out" && session.outgoing }.orEmpty()
    return when (session.phase) {
        PairPhase.CONNECTING -> PairingView(who, dir, "connecting")
        PairPhase.SHOW_CODE -> PairingView(who, "out", if (code.isNotEmpty()) "code" else "connecting", code = code)
        PairPhase.ENTER_CODE -> PairingView(who, "in", if (session.wrongCode) "wrong" else "input",
            wrongs = (3 - session.attemptsLeft).coerceIn(0, 3), attemptsLeft = session.attemptsLeft)
        PairPhase.VERIFYING -> if (session.outgoing) PairingView(who, "out", if (code.isNotEmpty()) "code" else "connecting", code = code)
            else PairingView(who, "in", "verifying", input = typed, attemptsLeft = session.attemptsLeft)
        PairPhase.COMPLETED -> PairingView(who, dir, "success", code = code, input = if (session.outgoing) "" else typed)
        PairPhase.ENDED -> null
    }
}

/** A received text bubble whose body has not arrived says why, never an empty bubble. */
private fun TransferMessage.pendingTextStatus(self: Boolean): String = when {
    self || !kind.equals("text", true) -> ""
    sourceAvailability.equals("SourceOffline", true) -> "来源设备离线，暂时无法获取"
    sourceAvailability.equals("NoSuchItem", true) || sourceAvailability.equals("Evicted", true) -> "内容已失效"
    downloadState.equals("failed", true) -> "获取失败"
    else -> "正在获取…"
}

internal fun TransferMessage.toUi(state: AppState, devices: Map<String, Device>): MessageUi {
    val self = isOutgoing
    val sender = devices[senderId]
    val proxy = proxyId?.let { devices[it] }
    val k = when (kind.lowercase(Locale.US)) { "rich_text" -> "text"; else -> kind.lowercase(Locale.US) }
    val ext = displayName.substringAfterLast('.', "").uppercase(Locale.US)
    // Received content: the core's download status decides (mobile-core.md 「传输页文件卡片」 4).
    val status = downloadStatus.takeIf { !self }
    val complete = status == MobileDownloadStatus.DOWNLOADED
    val live = state.transferProgress[id]?.percent
    val progress = when {
        self -> if (deliveryState == MobileDeliveryState.SENDING) live ?: 0 else null
        complete -> 100
        live != null -> live
        status == MobileDownloadStatus.DOWNLOADING -> 0
        k == "image" -> 0
        else -> null
    }
    val detail = when (k) {
        "folder" -> itemCount?.let { "$it 个文件" } ?: formatSize(size)
        else -> listOf(ext, formatSize(size)).filter { it.isNotEmpty() }.joinToString(" · ")
    }
    return MessageUi(id, self, senderId, if (self) state.name else sender?.name ?: senderId.take(8),
        if (self) "android" else platformOf(sender?.platform ?: "other"), self || sender?.online == true,
        k, text ?: pendingTextStatus(self), displayName, detail,
        proxyName = proxy?.name ?: proxyId?.take(8), proxyPlatform = proxy?.let { platformOf(it.platform) } ?: proxyId?.let { "other" },
        proxyOnline = proxy?.online == true, proxyId = proxyId,
        progress = progress, sent = deliveryState != MobileDeliveryState.SENDING,
        createdMs = createdMs,
        imagePath = localPath?.takeIf { k == "image" && (self || complete) && java.io.File(it).isFile },
        textPending = text == null && !self && kind.equals("text", true),
        preview = imagePreview?.name?.lowercase(Locale.US),
        thumbnailPath = thumbnailPath?.takeIf { imagePreview == MobileImagePreview.THUMBNAIL },
        aspect = imageAspect(),
        copyable = ClipboardSink.canCopy(this),
        senderPrivileged = if (self) devices.values.any { it.isSelf && it.androidPrivileged } else sender?.let { it.androidPrivileged && it.online } == true,
        batch = transferBatch(self, senderId, k, operationId),
        sensitive = sensitive && k == "text",
        direct = directPeer != null, toName = if (directPeer == null) null else if (self) directPeerName ?: devices[directPeer]?.name else devices.values.firstOrNull { it.isSelf }?.name ?: state.name, ended = directEnd(self),
        failure = failureText(self),
        localPath = localPath?.takeIf { (k == "image" || k == "file") && (self || complete) && java.io.File(it).isFile },
        mime = mime, size = size,
        downloading = !self && !complete && (live != null || status == MobileDownloadStatus.DOWNLOADING),
        downloadFailure = "下载失败".takeIf { status == MobileDownloadStatus.DOWNLOAD_FAILED },
        unavailable = when (status) {
            MobileDownloadStatus.EXPIRED -> "expired"
            MobileDownloadStatus.SOURCE_OFFLINE -> "offline"
            else -> null
        })
}

/** Width / height of an image message from the core's picture size (own: the original; received: its thumbnail). */
internal fun TransferMessage.imageAspect(): Float? =
    if ((imageWidth ?: 0) > 0 && (imageHeight ?: 0) > 0) imageWidth!!.toFloat() / imageHeight!! else null

/**
 * Batch key of a conversation record: images, files and folders of one share or clipboard offer
 * (the core gives every item of an operation the same `operation_id`), per sender. Own messages
 * use "self" as the sender so a batch never mixes directions.
 */
internal fun transferBatch(self: Boolean, senderId: String, kind: String, operationId: ByteArray): String? =
    if (kind !in BATCH_KINDS || operationId.isEmpty() || operationId.all { it == 0.toByte() }) null
    else (if (self) "self" else senderId) + ":" + operationId.joinToString("") { "%02x".format(it) }

private val BATCH_KINDS = setOf("image", "file", "folder")

/**
 * Folder listings with each downloaded child's progress and path, and every page whose listing failed (with or
 * without entries already shown), so its folder view stops loading.
 */
internal fun folderModels(state: AppState): Map<Pair<Long, Long>, FolderUi> = (state.folderPages.keys + state.folderFailures.keys)
    .associate { key ->
        val page = state.folderPages[key]
        val failure = state.folderFailures[key]
        val folder = state.transferMessages.firstOrNull { it.id == key.folderMessageId }
        val children = state.folderChildren[key.folderMessageId].orEmpty()
        (key.folderMessageId to key.directory) to FolderUi(folder?.displayName.orEmpty(), page?.entries.orEmpty().map { entry ->
            // The children list follows every change (announcements); the listing's snapshot fills in until it has the entry.
            val child = children.firstOrNull { it.directoryNode == entry.id } ?: entry.child
            folderItem(entry, child, child?.let { state.transferProgress[it.id]?.percent })
        }, key.folderMessageId, key.directory, hasMore = page?.nextOffset != null, error = failure?.text,
            retryable = failure?.retryable == true)
    }

/**
 * A folder listing row from its entry and its child record, if any. A child is not always a download:
 * a picture whose thumbnail was asked for has one too; the core's download status says which.
 * Pictures show the computer's thumbnail ([FolderItemUi.thumbnailPath]) until the file itself is here.
 */
internal fun folderItem(entry: DirectoryEntry, child: TransferMessage?, live: Int?): FolderItemUi {
    val dir = entry.kind == "directory"
    val status = child?.downloadStatus
    val done = status == MobileDownloadStatus.DOWNLOADED
    val image = !dir && (child?.mime?.startsWith("image/") == true || entry.name.substringAfterLast('.', "").lowercase(Locale.US) in IMAGE_EXTENSIONS)
    return FolderItemUi(entry.id.toString(), if (dir) "folder" else if (image) "image" else "file", entry.name,
        if (dir) "" else entry.size?.let { formatSize(it) }.orEmpty(),
        progress = when {
            done -> 100
            live != null -> live
            status == MobileDownloadStatus.DOWNLOADING -> 0
            else -> null
        },
        entryId = entry.id, path = child?.localPath?.takeIf { done },
        size = entry.size ?: 0L, mime = child?.mime,
        failure = "下载失败".takeIf { status == MobileDownloadStatus.DOWNLOAD_FAILED },
        thumbnailPath = child?.thumbnailPath?.takeIf { image })
}

private val IMAGE_EXTENSIONS = setOf("png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "bmp", "tif", "tiff")

private fun formatSize(n: Long): String = when {
    n <= 0 -> ""
    n < 1000 -> "$n B"
    n < 1_000_000 -> "${Math.round(n / 1000.0)} KB"
    n < 1_000_000_000 -> String.format(Locale.US, "%.1f MB", n / 1e6)
    else -> String.format(Locale.US, "%.1f GB", n / 1e9)
}

/** The share card shows only for the system share entry (ShareActivity), as the extension card. */
internal fun ShareUiState.toCard(share: Boolean): ShareCardUi? = if (!share) null else when (phase) {
    SharePhase.SENDING -> if (totalBytes == null) ShareCardUi("indeterminate", to = to?.takeIf { toPhone })
        else ShareCardUi("sending", ((progress ?: 0f) * 100).toInt(), rateBytesPerSecond, to = to?.takeIf { toPhone })
    SharePhase.SUCCEEDED -> ShareCardUi("done", 100, elapsed = elapsedSeconds ?: 0.0, to = to?.takeIf { toPhone })
    SharePhase.WAITING -> ShareCardUi("waiting", to = to?.takeIf { toPhone })
    else -> null
}

private fun localAddresses(): List<String> = runCatching {
    NetworkInterface.getNetworkInterfaces().toList().filter { it.isUp && !it.isLoopback }.flatMap { it.inetAddresses.toList() }
        .filter { !it.isLinkLocalAddress && !it.isLoopbackAddress }.map { it.hostAddress.orEmpty().substringBefore('%') }
        .sortedBy { if (it.contains(':')) 1 else 0 }
}.getOrDefault(emptyList())

/** Copies a downloaded image into Pictures/Oink, or a video into Movies/Oink; returns the toast text. */
private suspend fun saveToGallery(context: android.content.Context, path: String, name: String, mime: String?, video: Boolean): String = withContext(Dispatchers.IO) {
    runCatching {
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, name)
            (mime ?: fileMime(name))?.let { put(MediaStore.MediaColumns.MIME_TYPE, it) }
            put(MediaStore.MediaColumns.RELATIVE_PATH, if (video) "Movies/Oink" else "Pictures/Oink")
            put(MediaStore.MediaColumns.IS_PENDING, 1)
        }
        val collection = if (video) MediaStore.Video.Media.EXTERNAL_CONTENT_URI else MediaStore.Images.Media.EXTERNAL_CONTENT_URI
        val uri = context.contentResolver.insert(collection, values) ?: error("无法创建相册项目")
        try {
            context.contentResolver.openOutputStream(uri).use { out -> checkNotNull(out); java.io.File(path).inputStream().use { it.copyTo(out) } }
            context.contentResolver.update(uri, ContentValues().apply { put(MediaStore.MediaColumns.IS_PENDING, 0) }, null, null)
        } catch (e: Exception) { context.contentResolver.delete(uri, null, null); throw e }
    }.fold(onSuccess = { "已保存到相册" }, onFailure = { "无法保存到相册：${it.oinkMessage ?: "未知错误"}" })
}

private fun fileMime(name: String): String? =
    android.webkit.MimeTypeMap.getSingleton().getMimeTypeFromExtension(name.substringAfterLast('.', "").lowercase(Locale.US))

/** The media viewer's 分享: the system share sheet with the file (FileProvider URI). */
private fun shareLocalFile(context: android.content.Context, file: java.io.File, mime: String?) {
    val uri = ClipboardSink.uri(context, file)
    val intent = Intent(Intent.ACTION_SEND).setType(mime ?: fileMime(file.name) ?: "*/*").putExtra(Intent.EXTRA_STREAM, uri)
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    intent.clipData = android.content.ClipData.newUri(context.contentResolver, file.name, uri)
    context.startActivity(Intent.createChooser(intent, null).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
}

/** The media viewer's 用其他应用打开 (content it could not render): the system's ACTION_VIEW. */
private fun viewLocalFile(context: android.content.Context, file: java.io.File, mime: String?) {
    val uri = ClipboardSink.uri(context, file)
    context.startActivity(Intent(Intent.ACTION_VIEW).setDataAndType(uri, mime ?: fileMime(file.name) ?: "*/*")
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK))
}

/**
 * How a phone-to-phone transfer ended early, for 「对方已断开，…失败」 / 「已取消」: an own one the
 * other phone cancelled or lost, a received one that failed or was cancelled. Null otherwise
 * (an own share cancelled here is removed on both sides; other failures show their reason).
 */
internal fun TransferMessage.directEnd(self: Boolean): String? = when {
    // Received: the core's download status tells a failed or cancelled phone-to-phone receive apart.
    !self -> when (downloadStatus) {
        MobileDownloadStatus.RECEIVE_FAILED -> "failed"
        MobileDownloadStatus.CANCELLED -> "cancelled"
        else -> null
    }
    directPeer == null -> null
    deliveryState == MobileDeliveryState.FAILED -> when (deliveryFailure) {
        MobileDeliveryFailure.CANCELLED -> "cancelled"
        MobileDeliveryFailure.PEER_DISCONNECTED -> "failed"
        else -> null
    }
    else -> null
}

/**
 * Why a message failed, beside [directEnd]: an own send's reason, and a received phone-to-phone
 * transfer's reason when it was not the other side going away. A phone target that was offline
 * says so (the core's NoTarget otherwise means no computer).
 */
private fun TransferMessage.failureText(self: Boolean): String? = when {
    directEnd(self) == "cancelled" -> null
    self && deliveryState == MobileDeliveryState.FAILED && directEnd(self) == null ->
        if (directPeer != null && deliveryFailure == MobileDeliveryFailure.NO_TARGET) "对方不在线" else deliveryFailureText(deliveryFailure) ?: "发送失败"
    !self && directEnd(self) == "failed" && deliveryFailure != MobileDeliveryFailure.PEER_DISCONNECTED -> deliveryFailureText(deliveryFailure)
    else -> null
}

/** Badge of a transfer target: the member's own platform (iPad vs iPhone) when known, else its OS. */
private fun targetPlatform(id: String, os: uniffi.oink_mobile_ffi.CardOs, devices: Map<String, Device>) = devices[id]?.platform?.let(::platformOf)
    ?: when (os) {
        uniffi.oink_mobile_ffi.CardOs.ANDROID -> "android"; uniffi.oink_mobile_ffi.CardOs.IOS -> "iphone"
        uniffi.oink_mobile_ffi.CardOs.MACOS -> "mac"; uniffi.oink_mobile_ffi.CardOs.WINDOWS -> "windows"; else -> "other"
    }

internal fun uniffi.oink_mobile_ffi.TransferTarget.toPeer(devices: Map<String, Device>) =
    PeerUi(device, name, targetPlatform(device, os, devices), privileged = androidPrivileged)

/** The core's targets for the composer and the share picker. */
internal fun uniffi.oink_mobile_ffi.TransferTargetsView.toUi(devices: Map<String, Device>) = TransferTargetsUi(
    online = online.map { it.toPeer(devices) },
    activeDevice = activeDevice,
    otherDevices = otherDevices.toInt(),
    only = onlyDevice?.let { PeerUi(it.device, it.name, targetPlatform(it.device, it.os, devices), offline = !it.online) },
    searching = searching,
)

@androidx.compose.runtime.Composable
private fun ShareTheme(prefs: Prefs, systemDark: Boolean, content: @androidx.compose.runtime.Composable () -> Unit) {
    val dark = when (prefs.scheme) { "dark" -> true; "light" -> false; else -> systemDark }
    OinkTheme(androidx.compose.runtime.remember(dark, prefs.accent) { OinkColors.of(dark, prefs.accent) }, content)
}

/**
 * The share picker with its live state: a group with one other device skips it; a tap on a cached
 * device confirms it online first, and a device found not online leaves the list with 「X 不在线」.
 */
@androidx.compose.runtime.Composable
private fun SharePickerFlow(targets: TransferTargetsUi, connected: Boolean, repository: OinkRepository, summary: String, knownBytes: Long?, routingReady: Boolean,
                            onPick: (PeerUi, Boolean) -> Unit, onDismiss: () -> Unit) {
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    var connecting by androidx.compose.runtime.remember { androidx.compose.runtime.mutableStateOf<String?>(null) }
    var removed by androidx.compose.runtime.remember { androidx.compose.runtime.mutableStateOf(emptySet<String>()) }
    val toast = rememberToast()
    val cached = androidx.compose.runtime.remember { repository.cachedTargets().map { it.toPeer(emptyMap()) } }
    // One other device in the group (online or not): no choice to make. While it is offline the core
    // waits for it and the card says so (user 2026-11-11).
    val automatic = targets.only ?: if (routingReady) automaticShareTarget(targets, knownBytes) else null
    androidx.compose.runtime.LaunchedEffect(connected, automatic) {
        if (connected) automatic?.let { onPick(it, targets.only != null) }
    }
    // A small share cannot conclude "phones only" before the initial connect round ends.
    val findingSmallSharePc = targets.searching && knownBytes != null && knownBytes >= 0 && knownBytes < 300_000_000L
    if (!connected || !routingReady || automatic != null || findingSmallSharePc) {
        ShareCardLayer(ShareCardUi("indeterminate"), onCancel = onDismiss)
        return
    }
    SharePicker(summary, targets.searching, pickRows(cached, targets.online, targets.searching, removed), connecting, toast.value,
        onPick = { row ->
            if (connecting != null) return@SharePicker
            if (row.confirmed) { onPick(row.peer, false); return@SharePicker }
            connecting = row.peer.id
            scope.launch {
                val online = repository.confirmOnline(row.peer.id)
                connecting = null
                if (online) onPick(row.peer, false)
                else { removed = removed + row.peer.id; toast.value = ToastData(appText("${row.peer.name} 不在线", "${row.peer.name} is offline")) }
            }
        }, onDismiss = onDismiss)
}
