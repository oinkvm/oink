package com.hachiwii.oink

import kotlinx.coroutines.flow.Flow
import java.io.File
import uniffi.oink_mobile_ffi.AddressNetworkProblem
import uniffi.oink_mobile_ffi.CardActionUnavailable
import uniffi.oink_mobile_ffi.CardCommand
import uniffi.oink_mobile_ffi.CardCondition
import uniffi.oink_mobile_ffi.GameModePreference
import uniffi.oink_mobile_ffi.MicrophoneMode
import uniffi.oink_mobile_ffi.MobileDeliveryFailure
import uniffi.oink_mobile_ffi.MobileDeliveryState
import uniffi.oink_mobile_ffi.MobileEndpointState
import uniffi.oink_mobile_ffi.ResourceMode
import uniffi.oink_mobile_ffi.SpeakerActivity
import uniffi.oink_mobile_ffi.TransferFailure

enum class VolumeKind { Forwarding, Output }
data class VolumeControl(val title: String, val token: String, val volume: Int, val muted: Boolean, val kind: VolumeKind)
data class BlockedDevice(val id: String, val name: String, val platform: String = "other", val fingerprint: String = "", val blockedAtMs: Long = 0)
data class DeviceAction(val title: String, val token: String)

/**
 * One owner command from a device card. `token` is null when the command is not available
 * right now; `unavailable` then says why, or `needsPermissions` asks the UI to open the
 * owner's permission page. Invoke a token with [OinkRepository.invokeDeviceAction].
 */
data class ControlAction(
    val token: String?,
    val command: CardCommand? = null,
    val unavailable: CardActionUnavailable? = null,
    val needsPermissions: Boolean = false,
)

/** Screen, mouse, keyboard or clipboard of a PC. `mode == null`: the owner has not published it. */
data class ResourceState(
    val mode: ResourceMode?,
    val condition: CardCondition,
    /** Single tap: Shared <-> Local, Disabled -> Shared. */
    val tap: ControlAction,
    val shared: ControlAction,
    val local: ControlAction,
    /** Mouse and keyboard only. */
    val disabled: ControlAction? = null,
    /** Keyboard only: leave Disabled without choosing Shared/Local. */
    val enabled: ControlAction? = null,
)

data class SpeakerState(
    /** This PC is the group audio exit (ready or playing). */
    val isExit: Boolean,
    val activity: SpeakerActivity,
    /** MakeAudioExit when not the exit, ClearAudioExit when it is (see `command`). */
    val exitToggle: ControlAction,
    val forwardingOn: ControlAction,
    val forwardingOff: ControlAction,
    /** Forwarding gain 0..200 for this PC as a forwarding source. */
    val forwardingVolume: VolumeControl?,
    /** The PC's own 转发 setting (ForwardingEnabled); null while it has not been published. */
    val forwardingEnabled: Boolean? = null,
)

data class MicrophoneState(
    val mode: MicrophoneMode,
    val share: ControlAction,
    val local: ControlAction,
    val receive: ControlAction,
    /** Receiver slider: changes the sending device's gain (0..200). */
    val volume: VolumeControl?,
)

data class GameState(
    val preference: GameModePreference?,
    /** Game mode currently in effect (meaningful for Auto). */
    val active: Boolean?,
    val detected: Boolean?,
    val auto: ControlAction,
    val on: ControlAction,
    val off: ControlAction,
)

/** Group pointer lock. `toggle` locks to this PC, or unlocks when `locked`. */
data class FocusLockState(val locked: Boolean, val lockedElsewhere: Boolean, val toggle: ControlAction)

/** Current modes and the owner commands of one PC card (null for phones). */
data class DeviceControls(
    val screen: ResourceState,
    val mouse: ResourceState,
    val keyboard: ResourceState,
    val clipboard: ResourceState,
    val speaker: SpeakerState,
    val microphone: MicrophoneState,
    val game: GameState,
    val focus: FocusLockState,
)

/** Speaker/microphone endpoint of a phone (this phone included). */
data class MobileAudioState(val speaker: MobileEndpointState, val microphone: MobileEndpointState, val forward: MobileEndpointState = MobileEndpointState.LOCAL)

enum class DeviceConnection { THIS_DEVICE, ONLINE, CONNECTING, DISCONNECTED, ADDRESS_STALE, PROTOCOL_INCOMPATIBLE }

data class SocketEndpoint(val address: String, val port: Int)

data class Device(val id: String, val name: String, val online: Boolean, val computer: Boolean = false,
    val actions: List<DeviceAction> = emptyList(), val pairTarget: String? = null, val detail: String = "",
    val isSelf: Boolean = false, val platform: String = "other", val latencyMillis: Double? = null, val lastSeenMillis: Long? = null, val volumes: List<VolumeControl> = emptyList(),
    /** PC-style `XXXX-XXXX-XXXX` device ID. */
    val fingerprint: String = "",
    val connection: DeviceConnection? = null,
    /** "版本过低，请升级": typed from the card (ProtocolIncompatible) or the nearby row. */
    val incompatible: Boolean = false,
    val paused: Boolean? = null,
    val pause: ControlAction? = null,
    val controls: DeviceControls? = null,
    val mobileAudio: MobileAudioState? = null,
    /** Nearby rows: the address pairing will use, all addresses, and the group size. */
    val address: SocketEndpoint? = null,
    val addresses: List<SocketEndpoint> = emptyList(),
    val groupSize: Int? = null,
    /** Android device (this phone included) whose privileged mode is on and running: the crowned badge. */
    val androidPrivileged: Boolean = false)

data class PairRequest(val id: String, val name: String, val revision: String, val sourceCount: Int, val targetCount: Int,
    val deviceId: String = "", val platform: String = "other")

/** Pairing session phase (core `PairView.phase`). */
enum class PairPhase { CONNECTING, SHOW_CODE, ENTER_CODE, VERIFYING, COMPLETED, ENDED;
    companion object {
        fun of(value: UInt) = when (value) {
            0u -> CONNECTING
            1u -> SHOW_CODE
            2u -> ENTER_CODE
            in 3u..6u -> VERIFYING
            7u -> COMPLETED
            else -> ENDED
        }
    }
}

data class PairPeer(val deviceId: String, val name: String, val platform: String)

data class PairSession(val id: String, val revision: String, val attempt: UByte, val needsCode: Boolean, val detail: String = "",
    val outgoing: Boolean = false, val phase: PairPhase = PairPhase.CONNECTING, val attemptsLeft: Int = 0,
    /** The last code typed on either side was wrong. */
    val wrongCode: Boolean = false, val peer: PairPeer? = null)

/** Typed reason of a failed pairing or address query (core `PairProblem.code`). */
enum class PairFailureReason { DECLINED, CANCELLED_BY_PEER, TIMEOUT, SUPERSEDED, TOO_MANY_ATTEMPTS, WRONG_CODE,
    UNREACHABLE, INCOMPATIBLE, LINK_LOST, NOT_OINK, SELF_DEVICE, BUSY, OTHER;
    companion object {
        fun of(code: UInt) = when (code.toInt()) {
            13 -> DECLINED
            14 -> CANCELLED_BY_PEER
            6, 45 -> TIMEOUT
            23 -> SUPERSEDED
            20 -> TOO_MANY_ATTEMPTS
            19 -> WRONG_CODE
            4, 5 -> UNREACHABLE
            9, 10 -> INCOMPATIBLE
            16, 17 -> LINK_LOST
            7 -> NOT_OINK
            3 -> SELF_DEVICE
            11, 12, 46 -> BUSY
            else -> OTHER
        }
    }
}
data class PairFailure(val code: UInt, val stage: UInt, val reason: PairFailureReason)

/** Manual add ("手动添加") lookup state. */
sealed interface AddressLookup {
    data object Idle : AddressLookup
    data object Searching : AddressLookup
    data class Found(val device: Device) : AddressLookup
    /** `network` is always set by the core for address queries. */
    data class Failed(val network: AddressNetworkProblem?, val failure: PairFailure) : AddressLookup
}

/** Group audio exit and its system volume (0..100); `control` is null when it cannot be set. */
data class ExitState(val deviceId: String, val name: String, val volume: Int?, val muted: Boolean?, val control: VolumeControl?)

data class CoreSnapshot(
    val devices: List<Device> = emptyList(),
    val nearby: List<Device> = emptyList(),
    val requests: List<PairRequest> = emptyList(),
    val pairingCode: String? = null,
    val pairingSession: PairSession? = null,
    val blocked: List<BlockedDevice> = emptyList(),
    val foundAddress: Device? = null,
    val exitVolume: VolumeControl? = null,
    val managementStatus: List<String> = emptyList(),
    val compatibilityStatus: List<String> = emptyList(),
    val focuses: List<uniffi.oink_mobile_ffi.FocusSummary> = emptyList(),
    val addressLookup: AddressLookup = AddressLookup.Idle,
    val exit: ExitState? = null,
)
data class Endpoint(val address: String, val port: Int, val scopeId: Int)
data class Advertisement(val instance: String, val serviceType: String, val port: Int, val txt: Map<String, String>)
data class DiscoveredService(val key: String, val generation: Long, val txt: Map<String, String>, val endpoints: List<Endpoint>)
data class LocalItem(val path: String, val name: String, val mime: String, val size: Long)
/** [html]: the text's rich text, sent with it as its own representation (user 2026-11-12). */
data class ShareDraft(val text: String = "", val items: List<LocalItem> = emptyList(), val sensitive: Boolean = false, val html: String? = null)
/** [marker]: Oink clip marker bytes from the core, written next to the content unchanged (ClipMarker).
 * [html]: the text's rich text (the source HTML, or the core's HTML for RTF), written with it as one HTML clip. */
data class ReceivedContent(val text: String? = null, val items: List<LocalItem> = emptyList(), val sensitive: Boolean = false, val operationId: ULong = 0uL, val nativeRevision: ULong = 0uL, val marker: ByteArray? = null, val html: String? = null)
data class DeliveryResult(val target: String, val success: Boolean, val detail: String,
    val failure: MobileDeliveryFailure? = null)
/** [child]: the entry's record under its folder, when it has one (downloaded, downloading, or a picture
 * whose thumbnail was asked for; `MobileDirectoryEntry.child`). */
data class DirectoryEntry(val id: Long, val parent: Long?, val name: String, val kind: String, val size: Long?, val version: Long,
                          val child: TransferMessage? = null)
/** This phone's name: what the group sees, the system default, and whether the user overrode it. */
data class DeviceName(val name: String, val systemName: String, val custom: Boolean)
/** Ready to open with the system viewer (core auto-open slot). */
data class TransferOpen(val recordId: Long, val path: String, val displayName: String, val mime: String?)

sealed interface TransferCoreEvent {
    data class Message(val peerId: String, val message: String) : TransferCoreEvent
    data class Announcement(val recordId: Long, val contentId: String, val parentId: Long? = null, val autoOpenReady: Boolean = false) : TransferCoreEvent
    data class DownloadProgress(val operationId: ByteArray, val received: Long, val total: Long?, val recordId: Long) : TransferCoreEvent
    data class RateSample(val operationId: ByteArray, val bytesPerSecond: Long) : TransferCoreEvent
    data class SendProgress(val operationId: ByteArray, val sent: Long, val total: Long?, val recordIds: List<Long> = emptyList()) : TransferCoreEvent
    data class Delivery(val operationId: ByteArray, val status: String, val recordIds: List<Long> = emptyList(),
        val state: MobileDeliveryState? = null, val failure: MobileDeliveryFailure? = null) : TransferCoreEvent
    data class DirectoryPage(val directory: Long, val entries: List<DirectoryEntry>, val nextOffset: Int?,
        val folderMessageId: Long = 0, val nextVersion: Long? = null) : TransferCoreEvent
    /** The listing of [directory] (as requested, 0 = root) of folder [folderMessageId] ended without a page. */
    data class DirectoryFailed(val folderMessageId: Long, val directory: Long, val reason: TransferFailure,
        val retryable: Boolean, val detail: String) : TransferCoreEvent
    data class Error(val operationId: ByteArray?, val detail: String, val recordId: Long? = null) : TransferCoreEvent
    /** The core stored this phone's own records of a share (it emits no announcement for them). */
    data class OutgoingRecorded(val operationId: ByteArray) : TransferCoreEvent
    /** Where a transfer can go changed (contract 「目标设备列表」). */
    data class Targets(val view: uniffi.oink_mobile_ffi.TransferTargetsView) : TransferCoreEvent
    /** The core deleted these records: an own share that was cancelled, or a phone-to-phone transfer its sender cancelled. */
    data class Removed(val recordIds: List<Long>) : TransferCoreEvent
    /** A phone-to-phone transfer started arriving (its first record exists). */
    data class DirectIncoming(val senderName: String, val recordIds: List<Long>, val itemCount: Int, val firstName: String) : TransferCoreEvent
    /** A share sent with `waitForTarget` waits for its offline target ([waiting]), or the target came online and it starts now. */
    data class ShareWaiting(val operationId: ByteArray, val waiting: Boolean) : TransferCoreEvent
    /** A phone-to-phone receive ended (completed, failed or cancelled); its records carry the outcome. */
    data class DirectEnded(val recordIds: List<Long>) : TransferCoreEvent
}
sealed interface CoreEvent {
    data object Stopped : CoreEvent
    data class Snapshot(val value: CoreSnapshot) : CoreEvent
    data class Advertise(val value: Advertisement) : CoreEvent
    data class Available(val automaticTextAllowed: Boolean) : CoreEvent
    data class Received(val content: ReceivedContent) : CoreEvent
    data class Progress(val target: String, val bytes: Long, val total: Long?) : CoreEvent
    data class Transfer(val event: TransferCoreEvent) : CoreEvent
    data class Failure(val message: String) : CoreEvent
    /** "已与 X 配对" (no undo, decision 38). */
    data class PairingCompleted(val peer: PairPeer) : CoreEvent
    data class PairingFailed(val failure: PairFailure) : CoreEvent
    data class NameChanged(val value: DeviceName) : CoreEvent
    /**
     * The core's network layer failed (user 2026-11-12): this core cannot recover. The running
     * ConnectionService stops it and starts a new one through its normal start after
     * [retryAfterMillis] (the core's back-off). [detail] is diagnostic text.
     */
    data class NetworkFailed(val retryAfterMillis: Long, val detail: String) : CoreEvent
}

/** Typed result of an owner edit whose failure the UI shows in place (edit IP, block list). */
sealed interface EditResult {
    data object Ok : EditResult
    /** `code` is the core's `UserFacingCode` (InvalidAddress, NotMember, PersistenceFailed, ...). */
    data class Rejected(val code: uniffi.oink_mobile_ffi.UserFacingCode?, val detail: String) : EditResult
    /** Block list: `code` 0 = this device, 1 = persistence failed, 2 = limit reached. */
    data class BlockRejected(val code: UInt, val limit: UInt?) : EditResult
}

/** Suspend calls run off the UI thread. Publication checks are memory-only on Main.
 * stop must await sockets and tasks before returning. */
interface CoreBridge {
    /** Current in-process owner identity, unavailable while stopped/locked. */
    fun bootstrapDeviceId(): String? = null
    fun bootstrapIdentityProof(nonce: String, serial: String, digest: String, enabled: Boolean, revision: String): String? = null
    val events: Flow<CoreEvent>
    /** `systemName` is the system device name, the default this phone shows to the group. */
    suspend fun start(directory: File, systemName: String)
    fun networkChanged(generation: Long)
    /** Reopens the core's endpoint socket on the network the process is bound to now. */
    suspend fun rebindNetwork()
    suspend fun stop()
    /** Memory-only discovery hints; called on Main in NSD callback order. */
    fun discovered(service: DiscoveredService)
    fun lost(key: String, generation: Long)
    suspend fun automaticText(value: Boolean)
    /** Requests a fresh privileged native clipboard observation; the core owns the v2 announcement. */
    suspend fun privilegedReadAndBroadcast() {}
    suspend fun privilegedAutoRead(enabled: Boolean) {}
    /**
     * The 粘贴 tile in privileged mode: this phone's clipboard text read through the helper now, on the
     * caller's thread (off the main thread); independent of [privilegedAutoRead]. Null when it is not readable as text.
     */
    fun privilegedPasteText(): ClipPaste.Text? = null
    suspend fun pair(target: String)
    suspend fun accept(request: PairRequest)
    suspend fun submitCode(session: PairSession, code: String)
    suspend fun reject(request: PairRequest)
    /** true when the pairing was cancelled; false when there was none or it had already committed. */
    suspend fun cancelPairing(): Boolean
    suspend fun deviceSettings(device: String): uniffi.oink_mobile_ffi.DeviceSettingsView
    suspend fun setInputTransform(device: String, value: uniffi.oink_mobile_ffi.InputTransform)
    suspend fun setScreenGain(screen: uniffi.oink_mobile_ffi.ScreenKey, gain: Double, remote: Boolean)
    suspend fun setScreenDisabled(device: String, disabled: Boolean)
    suspend fun setInputOption(device: String, option: String, enabled: Boolean)
    suspend fun setFocus(value: uniffi.oink_mobile_ffi.FocusSummary, target: String?, locked: Boolean)
    suspend fun control(actionToken: String)
    suspend fun leaveGroup()
    suspend fun refreshNearby()
    suspend fun queryAddress(address: String, port: Int?)
    suspend fun releaseAddress()
    suspend fun block(device: String): EditResult
    suspend fun unblock(device: String): EditResult
    suspend fun renameDevice(device: String, name: String)
    suspend fun removeMember(device: String)
    suspend fun pauseAll()
    suspend fun setVolume(control: VolumeControl, volume: Int, muted: Boolean)
    suspend fun memberAddresses(device: String): List<String>
    suspend fun setMemberAddress(device: String, address: String): EditResult
    /** Sets (non-blank) or clears (null: back to the system name) this phone's custom name. */
    suspend fun setLocalDeviceName(name: String?): DeviceName
    fun localDeviceName(): DeviceName
    fun nativeClipboardChanged()
    fun validateReceived(value: ReceivedContent): Boolean
    fun reportPublish(value: ReceivedContent, success: Boolean)
    suspend fun publishPrivileged(value: ReceivedContent): com.hachiwii.oink.clipboard.NativeClipboardWrite =
        com.hachiwii.oink.clipboard.NativeClipboardWrite.Unavailable
    /**
     * The group's current shared clipboard (text, or an image the core downloaded) to copy to this phone;
     * null when there is nothing copyable (empty, source offline, no computer, file / folder / rich text).
     */
    suspend fun receive(): ReceivedContent?
    suspend fun cancelTransfers()
    /** Cancels an own share in progress by its operation (16 bytes); its records go away on both sides. */
    suspend fun cancelShare(operationId: ByteArray)
    /** Ends the phone-to-phone receive [messageId] belongs to; both sides show it cancelled. */
    suspend fun cancelReceive(messageId: Long)
    /** Where a transfer can go now (the composer and the share picker). */
    fun transferForeground(value: Boolean) {}
    fun transferTargets(): uniffi.oink_mobile_ffi.TransferTargetsView
    /** Reaches [device] now: true once it is online, false when this attempt ends without it. */
    suspend fun confirmOnline(device: String): Boolean
    /** [target]: the device the user chose (a computer forwards to the group, a phone gets it alone);
     * null: automatic. Text is broadcast whatever the target. [label] names the destination in the
     * result. [operationId] (16 bytes) is the share's, so it can be cancelled while it runs.
     * [waitForTarget]: the group's only other device, shared to without a picker (user 2026-11-11):
     * while it is offline the core records nothing and waits for it ([TransferCoreEvent.ShareWaiting]),
     * then starts by itself; 取消 meanwhile ends it unrecorded. */
    suspend fun share(draft: ShareDraft, target: String?, label: String, operationId: ByteArray,
                      waitForTarget: Boolean = false): List<DeliveryResult>
    suspend fun download(message: TransferMessage, open: Boolean)
    /** An image bubble became visible: ask its PC for a thumbnail; false when the core needs none. */
    suspend fun requestThumbnail(messageId: Long): Boolean = false
    /** Downloads one file of a folder message; returns the child record id. */
    suspend fun downloadFolderFile(folderMessageId: Long, entry: DirectoryEntry, open: Boolean): Long
    /** A picture's row of a computer's folder became visible: ask for its thumbnail; the child record
     *  it lands on, or null when the core fetches none (not a picture, or a folder from a phone). */
    suspend fun requestFolderThumbnail(folderMessageId: Long, entry: DirectoryEntry): Long? = null
    suspend fun listDirectory(message: TransferMessage, node: Long, cursorVersion: Long?, cursorOffset: Int?)
    /** Transfer history through the running session's own store. */
    val history: TransferHistory
    fun takeAutoOpen(): TransferOpen?
    fun cancelAutoOpen()
    /** Audio options persisted by the core (single source of truth). */
    fun audioSettings(): AudioSettings
    fun setAudioIdleTimeout(kind: uniffi.oink_mobile_ffi.MobileMediaKind, value: AudioTimeout)
    fun setEchoCancellation(enabled: Boolean)
    /** This phone's own microphone gain while it sends (0..200). */
    fun microphoneGain(): MicrophoneGain
    fun setMicrophoneGain(volume: Int, muted: Boolean): MicrophoneGain
}

data class AudioSettings(val microphoneTimeout: AudioTimeout, val speakerTimeout: AudioTimeout, val echoCancellation: Boolean)
data class MicrophoneGain(val volume: Int, val muted: Boolean, val revision: Long)

object SharePolicy {
    const val TEXT_LIMIT = 32 * 1024

    /** Only text is broadcast to every online device (protocol: directed or broadcast, broadcast is text only). */
    fun canBroadcast(draft: ShareDraft): Boolean =
        draft.items.isEmpty() && draft.text.isNotEmpty() && draft.text.toByteArray(Charsets.UTF_8).size.toLong() + (draft.html?.toByteArray(Charsets.UTF_8)?.size?.toLong() ?: 0L) <= TEXT_LIMIT
}
