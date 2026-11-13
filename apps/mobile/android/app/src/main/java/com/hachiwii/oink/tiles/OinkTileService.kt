package com.hachiwii.oink.tiles

import android.Manifest
import android.app.PendingIntent
import android.content.pm.PackageManager
import android.os.Build
import android.service.quicksettings.TileService
import androidx.core.content.ContextCompat
import com.hachiwii.oink.ConnectionService
import com.hachiwii.oink.AUDIO_MICROPHONE
import com.hachiwii.oink.CopyOutcome
import com.hachiwii.oink.LaunchRequest
import com.hachiwii.oink.OinkApplication
import com.hachiwii.oink.mainLaunchIntent
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Oink's Quick Settings tiles (user 2026-11-11, native-rewrite.md 「控制中心快捷开关」). Non-active
 * tiles: Android binds them whenever the panel opens, so a tile never shows a state left over from a
 * killed process; while the panel is open the tile follows the repository state live.
 */
abstract class OinkTileService(private val kind: TileKind) : TileService() {
    private val repository get() = (application as OinkApplication).repository
    private val scope = MainScope()
    private var listening: Job? = null

    private fun facts() = tileFacts(repository.state.value, ConnectionService.inForeground, microphonePermission())
    private fun microphonePermission() =
        ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

    override fun onStartListening() {
        super.onStartListening()
        listening?.cancel()
        listening = scope.launch {
            combine(repository.state, TileFeedback.texts) { state, feedback ->
                tileLook(kind, tileFacts(state, ConnectionService.inForeground, microphonePermission()), feedback[kind])
            }.distinctUntilChanged().collect { render(it) }
        }
    }

    override fun onStopListening() {
        listening?.cancel()
        listening = null
        super.onStopListening()
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }

    private fun render(look: TileLook) {
        val tile = qsTile ?: return
        tile.label = kind.label
        tile.state = look.state
        tile.subtitle = look.subtitle
        tile.updateTile()
    }

    override fun onClick() {
        super.onClick()
        // Turning the microphone on or off from a locked phone asks for the unlock first (user 2026-11-11).
        if (kind == TileKind.MICROPHONE && isLocked) unlockAndRun { handle() } else handle()
    }

    private fun handle() {
        when (val click = tileClick(kind, facts(), java.util.UUID.randomUUID().toString())) {
            TileClick.Ignore -> Unit
            TileClick.Copy -> copy()
            is TileClick.PrivilegedPaste -> privilegedPaste(click.fallback)
            is TileClick.SetForward -> repository.setLocalForward(click.on)
            is TileClick.SetSpeaker -> repository.setLocalSpeaker(click.on)
            is TileClick.SetMicrophone -> setMicrophone(click)
            is TileClick.Open -> open(click.request)
        }
    }

    /** In the background; the panel stays open and the subtitle shows the result. Never opens Oink. */
    private fun copy() {
        TileFeedback.show(kind, "获取中")
        repository.scope.launch {
            val text = when (repository.copySharedClipboard().await()) {
                CopyOutcome.Copied -> "已复制"
                CopyOutcome.Nothing -> "没有内容"
                is CopyOutcome.Failed -> "复制失败"
            }
            TileFeedback.flash(kind, text)
        }
    }

    /**
     * Privileged mode: text is read through the helper and sent in the background, whatever 「自动读取」
     * says (user 2026-11-11). Anything the helper cannot give as text (an image, too large, locked,
     * helper not running) opens 传输, which reads and sends it with the window focus.
     */
    private fun privilegedPaste(fallback: LaunchRequest) {
        scope.launch {
            val text = withContext(Dispatchers.IO) { repository.privilegedPasteText() }
            if (text == null) { open(fallback); return@launch }
            TileFeedback.show(kind, "发送中")
            repository.pasteClipboard(text) { failure -> TileFeedback.flash(kind, if (failure == null) "已发送" else "发送失败") }
        }
    }

    /**
     * Turning the microphone on adds the microphone type to the running foreground service right in the
     * tap; Android keeps the while-in-use eligibility the service got when it started on screen. When it
     * refuses, Oink opens on 设备 and turns it on there (with the permission prompt if needed).
     */
    private fun setMicrophone(click: TileClick.SetMicrophone) {
        if (click.on) {
            val added = try { ConnectionService.addAudioTypes(AUDIO_MICROPHONE) } catch (e: RuntimeException) {
                android.util.Log.w("OinkTile", "microphone type refused", e)
                false
            }
            if (!added) { open(click.fallback); return }
        }
        repository.setLocalMicrophone(click.on)
    }

    /** Opens Oink (collapsing the panel; the lock screen asks for the unlock first). */
    @Suppress("DEPRECATION")
    private fun open(request: LaunchRequest) {
        val intent = mainLaunchIntent(this, request)
        if (Build.VERSION.SDK_INT >= 34) {
            startActivityAndCollapse(PendingIntent.getActivity(this, kind.ordinal + 100, intent,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT))
        } else startActivityAndCollapse(intent)
    }
}

class CopyTileService : OinkTileService(TileKind.COPY)
class PasteTileService : OinkTileService(TileKind.PASTE)
class SpeakerTileService : OinkTileService(TileKind.SPEAKER)
class ForwardTileService : OinkTileService(TileKind.FORWARD)
class MicrophoneTileService : OinkTileService(TileKind.MICROPHONE)
