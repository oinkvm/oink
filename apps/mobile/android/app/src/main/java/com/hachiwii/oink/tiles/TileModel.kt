package com.hachiwii.oink.tiles

import android.service.quicksettings.Tile
import com.hachiwii.oink.AppState
import com.hachiwii.oink.HomeTab
import com.hachiwii.oink.LaunchRequest
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

/** Oink's Quick Settings tiles (user 2026-11-11, native-rewrite.md 「控制中心快捷开关」). */
enum class TileKind(val label: String) {
    /** One-shot: copy the group's shared clipboard to this phone. */
    COPY("复制"),
    /** One-shot: send this phone's clipboard (the transfer page's rules). */
    PASTE("粘贴"),
    /** Toggle: this phone is the group's audio exit. */
    SPEAKER("接收音频"),
    /** Toggle: this phone shares its microphone. */
    MICROPHONE("共享麦克风"),
    FORWARD("音频转发"),
}

/** What the tiles read, from the repository state and the running service. */
internal data class TileFacts(
    /** The connection service runs (its owner loop is attached). */
    val serviceRunning: Boolean,
    /** The group has another device. */
    val paired: Boolean,
    /** The core runs. */
    val connected: Boolean,
    /** The service is in the foreground with the persistent notification, so it works in the background. */
    val inForeground: Boolean,
    val privilegedReady: Boolean,
    val speakerOn: Boolean,
    val microphoneOn: Boolean,
    val microphonePermission: Boolean,
    val forwardOn: Boolean = false,
    val captureReady: Boolean = privilegedReady,
)

internal fun tileFacts(state: AppState, inForeground: Boolean, microphonePermission: Boolean) = TileFacts(
    serviceRunning = state.serviceEnabled,
    paired = state.snapshot.devices.any { !it.isSelf },
    connected = state.connected,
    inForeground = inForeground,
    privilegedReady = state.privilegedReady,
    speakerOn = state.localAudio.speakerOn,
    microphoneOn = state.localAudio.microphoneOn,
    microphonePermission = microphonePermission,
    forwardOn = state.localAudio.forwardOn,
    captureReady = state.captureReady,
)

/** [state]: a [Tile] state. [subtitle]: a short feedback or the reason it is unavailable. */
internal data class TileLook(val state: Int, val subtitle: String?)

internal const val TILE_NOT_RUNNING = "Oink 未运行"
internal const val TILE_NOT_PAIRED = "未配对"

/**
 * Unavailable (not clickable) while the service does not run or the phone is in no group; the toggles
 * are active while this phone holds the role, the one-shot tiles always inactive. [feedback] (获取中,
 * 已复制, …) shows for a moment after a tap.
 */
internal fun tileLook(kind: TileKind, facts: TileFacts, feedback: String?): TileLook = when {
    !facts.serviceRunning -> TileLook(Tile.STATE_UNAVAILABLE, TILE_NOT_RUNNING)
    !facts.paired -> TileLook(Tile.STATE_UNAVAILABLE, TILE_NOT_PAIRED)
    kind == TileKind.FORWARD && !facts.captureReady -> TileLook(Tile.STATE_UNAVAILABLE, "需特权模式")
    kind == TileKind.FORWARD && (!facts.connected || !facts.inForeground) -> TileLook(Tile.STATE_UNAVAILABLE, TILE_NOT_RUNNING)
    kind == TileKind.FORWARD -> TileLook(if (facts.forwardOn) Tile.STATE_ACTIVE else Tile.STATE_INACTIVE, if (facts.forwardOn) "转发中" else feedback)
    kind == TileKind.SPEAKER && facts.forwardOn -> TileLook(Tile.STATE_UNAVAILABLE, "转发中")
    kind == TileKind.SPEAKER -> TileLook(if (facts.speakerOn) Tile.STATE_ACTIVE else Tile.STATE_INACTIVE, feedback)
    kind == TileKind.MICROPHONE -> TileLook(if (facts.microphoneOn) Tile.STATE_ACTIVE else Tile.STATE_INACTIVE, feedback)
    else -> TileLook(Tile.STATE_INACTIVE, feedback)
}

/** What a tap does. */
internal sealed interface TileClick {
    /** The tile is unavailable (Android does not deliver such taps; kept for completeness). */
    data object Ignore : TileClick
    /** Copy the shared clipboard in the background. */
    data object Copy : TileClick
    /** Privileged mode: read the clipboard through the helper and send it in the background; open
     * [fallback] when it cannot be read as text. */
    data class PrivilegedPaste(val fallback: LaunchRequest) : TileClick
    /** Set the speaker directly (the service runs and may play in the background). */
    data class SetSpeaker(val on: Boolean) : TileClick
    data class SetForward(val on: Boolean) : TileClick
    /** Set the microphone directly; turning it on first adds the microphone type, [fallback] when refused. */
    data class SetMicrophone(val on: Boolean, val fallback: LaunchRequest) : TileClick
    /** Open Oink with [request] (a page, and a one-shot action it runs there). */
    data class Open(val request: LaunchRequest) : TileClick
}

/**
 * Routes a tap. The toggles set an explicit state (the opposite of what the tile shows): directly while
 * the service runs in the foreground with a running core (turning the microphone on also needs the
 * permission); otherwise Oink opens on 设备 and sets that state there. 粘贴 without privileged mode
 * always opens 传输, which reads the clipboard once it has the focus.
 */
internal fun tileClick(kind: TileKind, facts: TileFacts, requestId: String): TileClick {
    if (!facts.serviceRunning || !facts.paired) return TileClick.Ignore
    val paste = LaunchRequest(tab = HomeTab.TRANSFER, sendClipboard = true, id = requestId)
    return when (kind) {
        TileKind.COPY -> TileClick.Copy
        TileKind.PASTE -> if (facts.privilegedReady && facts.connected) TileClick.PrivilegedPaste(paste) else TileClick.Open(paste)
        TileKind.FORWARD -> if (facts.captureReady && facts.connected && facts.inForeground) TileClick.SetForward(!facts.forwardOn) else TileClick.Ignore
        TileKind.SPEAKER -> {
            if (facts.forwardOn) return TileClick.Ignore
            val on = !facts.speakerOn
            if (facts.connected && facts.inForeground) TileClick.SetSpeaker(on)
            else TileClick.Open(LaunchRequest(tab = HomeTab.DEVICES, speaker = on, id = requestId))
        }
        TileKind.MICROPHONE -> {
            val on = !facts.microphoneOn
            val open = LaunchRequest(tab = HomeTab.DEVICES, microphone = on, id = requestId)
            if (facts.connected && facts.inForeground && (!on || facts.microphonePermission)) TileClick.SetMicrophone(on, open)
            else TileClick.Open(open)
        }
    }
}

/** Short feedback under a tile after a tap; process-wide, since a tile service may be rebound meanwhile. */
internal object TileFeedback {
    private val mutable = MutableStateFlow<Map<TileKind, String>>(emptyMap())
    val texts: StateFlow<Map<TileKind, String>> = mutable.asStateFlow()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val clears = HashMap<TileKind, Job>()

    /** Shows [text] until the next feedback (获取中, 发送中). */
    fun show(kind: TileKind, text: String) = synchronized(clears) {
        clears.remove(kind)?.cancel()
        mutable.update { it + (kind to text) }
    }

    /** Shows [text] for 2 s (已复制, 没有内容, …). */
    fun flash(kind: TileKind, text: String) = synchronized(clears) {
        clears.remove(kind)?.cancel()
        mutable.update { it + (kind to text) }
        clears[kind] = scope.launch {
            delay(2000)
            mutable.update { it - kind }
        }
    }
}
