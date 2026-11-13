package com.hachiwii.oink.ui

/** Presentation models. OinkScreen maps the repository into these; DesignFixtures builds them for design scenes. */

enum class Tab(val label: String) { Devices("设备"), Transfer("传输"), Control("控制") }

/** mouse / keyboard: shared, local, blocked, unavailable. speaker: forward, outlet, local, unavailable. mic: send, receive, local, unavailable. */
data class PeerUi(
    val id: String, val name: String, val platform: String,
    val offline: Boolean = false, val latency: Double? = null, val lastSeen: String = "5 分钟",
    val incompatible: Boolean = false,
    val mouse: String = "local", val keyboard: String = "local",
    val speaker: String = "local", val mic: String = "local",
    val speakerLevel: Int = 65, val micLevel: Int = 65,
    val locked: Boolean = false, val gaming: String = "off", val paused: Boolean = false,
    /** Game mode currently in effect (what an "auto" preference resolved to). */
    val gamingActive: Boolean = false,
    /** The volume bars have a control behind them (otherwise dragging explains why not). */
    val speakerAdjustable: Boolean = true, val micAdjustable: Boolean = true,
    /** Online Android device in privileged mode (on and running): the crowned badge. */
    val privileged: Boolean = false,
) {
    val mobile get() = platform == "iphone" || platform == "ipad" || platform == "android"
    val computer get() = platform == "mac" || platform == "windows"
}

data class MessageUi(
    val id: Long, val self: Boolean, val senderId: String, val senderName: String, val senderPlatform: String, val senderOnline: Boolean,
    val kind: String, val text: String = "", val name: String = "", val detail: String = "",
    val proxyName: String? = null, val proxyPlatform: String? = null, val proxyOnline: Boolean = true, val proxyId: String? = null,
    /** Receiver side: null = not started, 0–100 while downloading, 100 = done. Sender side: send progress. */
    val progress: Int? = null, val sent: Boolean = true,
    /** Own message that could not be delivered: the reason. */
    val failure: String? = null,
    val createdMs: Long = 0,
    /** Downloaded (or own) image file, shown instead of the placeholder artwork. */
    val imagePath: String? = null,
    /** The long-press menu offers 复制: text, or an image whose data is downloaded (ClipboardSink.canCopy). */
    val copyable: Boolean = false,
    /** The sender is an Android device in privileged mode right now: its avatar wears the crown. */
    val senderPrivileged: Boolean = false,
    /**
     * Images, files and folders sent or received together (one share / clipboard offer, the same
     * operation id from the same sender) carry the same key; consecutive messages with one key show
     * as a stack ([transferRows]). Null for text and for content without an operation.
     */
    val batch: String? = null,
    /** A received text whose body has not arrived: [text] holds the reason, drawn as secondary text. */
    val textPending: Boolean = false,
    /** Image messages: original / thumbnail / loading / expired / offline / failed / placeholder (core ImagePreview). */
    val preview: String? = null,
    /** The PC's preview of a not-downloaded image. */
    val thumbnailPath: String? = null,
    /** Width / height of the image from the core (own: the original as sent; received: its thumbnail). */
    val aspect: Float? = null,
    /** Text copied on a computer with a password marker: shown as *** until the user taps 显示. */
    val sensitive: Boolean = false,
    /**
     * Phone-to-phone (native-rewrite.md 「手机点对点传输」): sent straight to one phone, or received
     * straight from one. Received direct content arrives by itself (no download button) and can be
     * cancelled while it moves.
     */
    val direct: Boolean = false,
    /** Own direct message: the phone it went to (「已发送给 X」). */
    val toName: String? = null,
    /** How a phone-to-phone transfer ended early: "failed" (a side disconnected) or "cancelled". */
    val ended: String? = null,
    /** The content's file on this phone (own, or downloaded completely) for images and files; null otherwise. */
    val localPath: String? = null,
    /** The content's MIME type as announced (null: unknown; the name's extension decides). */
    val mime: String? = null,
    /** The content's size in bytes (0: unknown). */
    val size: Long = 0,
    /** A download of this received content is running. */
    val downloading: Boolean = false,
    /** Received content whose last download failed: the reason (the media viewer shows it with 重试). */
    val downloadFailure: String? = null,
    /** Received content that cannot be fetched now: "expired" (gone from its source) or "offline" (source offline). */
    val unavailable: String? = null,
    /**
     * A text message's rich text as the core's cleaned markup (MobileRichText.displayHtml, user 2026-11-12): the
     * bubble shows it formatted and the menu adds 复制文本. Null: plain text (no rich text here yet, or it keeps
     * no formatting).
     */
    val richHtml: String? = null,
) {
    /** Own content still on its way (not text): long press offers only 取消发送. */
    val sendingNow get() = self && kind != "text" && !sent && failure == null && ended == null
    /** Direct content still arriving: long press offers 取消接收. */
    val receivingNow get() = !self && direct && ended == null && (progress ?: 0) < 100
    /** Tapping opens the viewer unless it could show nothing: the original is gone or its PC is offline. */
    val opensViewer get() = preview != "expired" && preview != "offline" && !(!self && ended != null)
}

/** One row of a folder listing; [path] is set once the file is on this phone (downloaded, or the folder came straight from a phone). */
data class FolderItemUi(val id: String, val kind: String, val name: String, val detail: String, val progress: Int? = null,
                        val entryId: Long = 0, val path: String? = null,
                        /** Size in bytes (0: unknown), MIME type once known, and why its last download failed (media viewer). */
                        val size: Long = 0, val mime: String? = null, val failure: String? = null,
                        /** A picture's thumbnail from its computer, until the file itself is here. */
                        val thumbnailPath: String? = null)
/**
 * A folder message's listing at [node] (0 = the folder itself). [error]: the listing (or its next page) ended
 * without a page; [retryable] offers 重试 for it.
 */
data class FolderUi(val name: String, val items: List<FolderItemUi>, val messageId: Long = 0, val node: Long = 0,
                    val hasMore: Boolean = false, val loading: Boolean = false, val error: String? = null,
                    val retryable: Boolean = false)

data class NearbyUi(val id: String, val name: String, val platform: String, val ip: String, val port: Int, val deviceId: String = "",
                    val incompatible: Boolean = false, val groupSize: Int = 1) {
    val address get() = if (ip.isEmpty()) "" else "$ip:$port"
}

/** One pairing session as the pairing sheet shows it. dir: out (this device shows the code) or in (types it). */
data class PairingView(
    val peer: NearbyUi, val dir: String,
    /** connecting, code, input, verifying, wrong, success, failed */
    val status: String, val code: String = "", val input: String = "", val wrongs: Int = 0, val reason: String = "",
    /** Attempts the core still allows; null in design scenes (they count [wrongs] of 3). */
    val attemptsLeft: Int? = null,
)

data class ManualState(val ip: String = "", val port: String = "", val error: String? = null, val errorField: String? = null,
                       val searching: Boolean = false, val found: NearbyUi? = null, val shake: Int = 0)

data class SelfInfo(val ips: List<String>, val port: Int?, val deviceId: String)

/** Settings values (defaultPrefs in SettingsPage.jsx). */
data class Prefs(
    val accent: Accent = Accent.Pink, val scheme: String = "system", val language: String = "system", val homeTab: Tab = Tab.Devices,
    val cacheBytes: Long = 0,
    val autoWriteClipboard: Boolean = false, val autoReadClipboard: Boolean = false,
    val speakerTap: com.hachiwii.oink.SpeakerTap = com.hachiwii.oink.SpeakerTap.EXIT,
    val micAutoOff: Int = 5, val speakerAutoOff: Int = 5, val echoCancel: Boolean = true,
    val persistentNotice: Boolean = true, val hideRecents: Boolean = false,
    /** 「点按通知」: copy (从共享剪贴板复制) or open (打开 App at [noticeTab]); user 2026-11-11. */
    val noticeTap: String = "copy", val noticeTab: Tab = Tab.Devices,
    val openWith: Boolean = false, val openWithGroups: Map<String, Boolean> = mapOf("image" to true, "video" to true, "audio" to true, "document" to true, "archive" to true, "apk" to false, "other" to false),
    /** off, setup, starting, on, failed */
    val privileged: String = "off", val autoRecover: Boolean = true,
    /** 特权模式: Oink draws the pointer instead of the system (Android 15+ only; below it always does). */
    /** Audio options live in the running core; false while it does not run. */
    val audioAvailable: Boolean = true,
)

/** [to]: the phone a share goes straight to (null: a computer). */
/**
 * The share card. [phase]: waiting / indeterminate / sending / done. [to]: the receiving phone (its
 * name in the text, a phone on the stage); null for a computer. [freezeT] / [freezeHandoff]: a fixed
 * flight phase / hand-over moment (ms after the target came online) for design scenes.
 */
data class ShareCardUi(val phase: String, val progress: Int = 0, val rate: Double? = null, val elapsed: Double = 0.0, val freezeT: Float? = null,
                       val to: String? = null, val freezeHandoff: Float? = null)

/** This phone's name: [name] is what the group sees, [systemName] the default it falls back to. */
data class DeviceNameUi(val name: String, val systemName: String, val custom: Boolean)

data class AppModel(
    val selfName: String = "Android",
    val peers: List<PeerUi> = emptyList(),
    val requests: List<NearbyUi> = emptyList(),
    val nearby: List<NearbyUi> = emptyList(),
    val self: SelfInfo = SelfInfo(emptyList(), 47470, ""),
    val messages: List<MessageUi> = emptyList(),
    val composer: String = "",
    val share: ShareCardUi? = null,
    val pairing: PairingView? = null,
    val manual: ManualState = ManualState(),
    val speakerOn: Boolean = false, val micOn: Boolean = false,
    val forwardOn: Boolean = false, val forwardFailed: Boolean = false,
    /** This phone's speaker / microphone could not start (shown in the warning colour). */
    val speakerFailed: Boolean = false, val micFailed: Boolean = false,
    val version: String = "",
    /** Port of this phone's wireless-debugging pairing service, once found. */
    val pairingPort: Int? = null,
    /**
     * The system's 「开发者选项」 / 「无线调试」 switches right now, for 特权模式's guide; null where not known
     * (design scenes, or the setting cannot be read): the guide then keeps the step it is on.
     */
    val developerOptionsOn: Boolean? = null, val wirelessDebuggingOn: Boolean? = null,
    /** null in design scenes: the settings row then shows [selfName] read-only. */
    val deviceName: DeviceNameUi? = null,
    /** Persistent notification off: Oink stops whenever it leaves the screen. */
    val worksInBackground: Boolean = true,
    /** Android 11–12: wireless debugging must be turned on again after a reboot. */
    val rebootNeedsWirelessDebugging: Boolean = false,
    /** Older transfer messages can still be loaded. */
    val hasOlderMessages: Boolean = false,
    val loadingOlder: Boolean = false,
    val olderLoadFailed: Boolean = false,
    val sharedBoundaryMs: Long? = null,
    /** Folder listings by folder message id and node. */
    val folders: Map<Pair<Long, Long>, FolderUi> = emptyMap(),
    /** Why privileged mode failed to start (the "启动失败" card). */
    val privilegedFailure: String? = null,
    /** Privileged mode is on but PCs cannot move this phone's pointer: the helper's reason. */
    val privilegedPointerProblem: String? = null,
    /** Where the transfer page can send (core `TransferTargetsView`); null in design scenes: derived from [peers]. */
    val targets: TransferTargetsUi? = null,
    /** Why the connection could not start; shown above the device list with 重试. */
    val connectionFailure: String? = null,
)

/** Initial navigation state for a design scene (and the defaults for a normal launch). */
data class UiInit(
    val tab: Tab = Tab.Devices,
    val addPage: Boolean = false,
    val scrollToEnd: Boolean = false,
    // device control sheet
    val sheetPeer: String? = null, val sheetFocus: String? = null, val sheetKick: Boolean = false, val sheetEditIp: Boolean = false,
    // transfer
    /** [transferProxy]: the device the composer sends to (the last choice). */
    val transferMenu: String? = null, val transferProxy: String? = null, val transferToast: String? = null,
    /** Open the transfer page at this message (a phone-to-phone notification was tapped). */
    val transferFocus: Long? = null,
    /** Message whose long-press menu is open. */
    val transferMessageMenu: Long? = null,
    /** Batches shown expanded instead of as a stack ([MessageUi.batch]). */
    val transferExpanded: Set<String> = emptySet(),
    val viewer: MessageUi? = null, val folder: FolderUi? = null,
    // settings
    val settings: Boolean = false, val settingsStack: List<String> = listOf("main"), val settingsDialog: String? = null,
    val settingsScroll: String? = null, val settingsToast: String? = null, val privStep: Int = 1,
    // add device
    val pairingSheet: String? = null, val toast: ToastData? = null,
    /** Design scenes keep timed notices and toasts on screen. */
    val freeze: Boolean = false,
    /** Control page scene state (docs/control-page.md §8). */
    val control: ControlInit? = null,
)
