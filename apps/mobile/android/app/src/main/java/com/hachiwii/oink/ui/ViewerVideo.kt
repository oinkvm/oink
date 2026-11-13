package com.hachiwii.oink.ui

import android.content.Context
import android.net.Uri
import android.view.TextureView
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectHorizontalDragGestures
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.ProgressBarRangeInfo
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.progressBarRangeInfo
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.setProgress
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.foundation.Canvas
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.common.VideoSize
import androidx.media3.exoplayer.ExoPlayer
import java.io.File
import java.util.Locale
import kotlinx.coroutines.delay

/** What the video page shows about its player (Compose state). */
@Stable
internal class VideoState {
    var playing by mutableStateOf(false)
    var positionMs by mutableLongStateOf(0L)
    var durationMs by mutableLongStateOf(0L)
    var muted by mutableStateOf(false)
    /** Width / height of the picture once known. */
    var aspect by mutableStateOf<Float?>(null)
    /** The file could not be played (codec / container the phone cannot read). */
    var failed by mutableStateOf(false)
}

/** One video page's player. Tests stand in a fake through [LocalVideoPlayers]. */
internal interface VideoController {
    val state: VideoState
    fun play()
    fun pause()
    fun seekTo(ms: Long)
    fun setMuted(muted: Boolean)
    /** Reads the position into [state] (polled while playing). */
    fun tick()
    fun release()
    @Composable fun Picture(modifier: Modifier)
}

/** Makes the player of a video page; ExoPlayer (Media3) outside tests. */
internal val LocalVideoPlayers = staticCompositionLocalOf<(Context, String) -> VideoController> { { context, path -> ExoVideoController(context, path) } }

/** Media3 ExoPlayer on a TextureView (it moves and scales with the pager and the open / close animation). */
private class ExoVideoController(context: Context, path: String) : VideoController {
    override val state = VideoState()
    private val player = ExoPlayer.Builder(context).build().apply {
        // Media playback with audio focus: another app's audio pauses it, and it pauses on unplugging headphones.
        setAudioAttributes(AudioAttributes.Builder().setUsage(C.USAGE_MEDIA).setContentType(C.AUDIO_CONTENT_TYPE_MOVIE).build(), true)
        setHandleAudioBecomingNoisy(true)
        setMediaItem(MediaItem.fromUri(Uri.fromFile(File(path))))
        addListener(object : Player.Listener {
            override fun onIsPlayingChanged(isPlaying: Boolean) { state.playing = isPlaying }
            override fun onPlaybackStateChanged(playbackState: Int) {
                if (playbackState == Player.STATE_READY) state.durationMs = duration.coerceAtLeast(0L)
                if (playbackState == Player.STATE_ENDED) state.positionMs = state.durationMs
            }
            override fun onVideoSizeChanged(videoSize: VideoSize) {
                if (videoSize.width > 0 && videoSize.height > 0) state.aspect = videoSize.width * videoSize.pixelWidthHeightRatio / videoSize.height
            }
            override fun onPlayerError(error: PlaybackException) { state.failed = true }
        })
        prepare()
    }
    override fun play() {
        // At the end, play starts over.
        if (player.playbackState == Player.STATE_ENDED) player.seekTo(0L)
        player.play()
    }
    override fun pause() = player.pause()
    override fun seekTo(ms: Long) { player.seekTo(ms); state.positionMs = ms }
    override fun setMuted(muted: Boolean) { player.volume = if (muted) 0f else 1f; state.muted = muted }
    override fun tick() { state.positionMs = player.currentPosition.coerceAtLeast(0L) }
    override fun release() = player.release()
    @Composable override fun Picture(modifier: Modifier) {
        AndroidView({ TextureView(it).also { view -> player.setVideoTextureView(view) } }, modifier,
            onRelease = { player.clearVideoTextureView(it) })
    }
}

/** How long the playing video's controls stay after the last touch. */
internal const val VIDEO_CONTROLS_MS = 2500L

/**
 * A video page: the picture fitted in the page, a big play / pause in the middle and a bar with
 * the time, a scrubber and mute above the save / share row. It plays when it is the settled page
 * (opening on it or paging to it) and pauses when paged away, while closing or when the app
 * leaves the screen. A tap shows or hides the controls; they hide by themselves while playing.
 */
@Composable
internal fun VideoPage(m: MessageUi, path: String, current: Boolean, settled: Boolean, closing: Boolean, transition: Modifier, onOpenWith: () -> Unit) {
    val context = LocalContext.current
    val players = LocalVideoPlayers.current
    val controller = remember(path) { players(context, path) }
    DisposableEffect(controller) { onDispose { controller.release() } }
    val s = controller.state
    var controls by remember { mutableStateOf(true) }
    var touched by remember { mutableIntStateOf(0) }
    LaunchedEffect(settled, closing) { if (settled && !closing) controller.play() else controller.pause() }
    LaunchedEffect(current) { if (!current) controller.pause() }
    LifecycleEventEffect(Lifecycle.Event.ON_PAUSE) { controller.pause() }
    LaunchedEffect(s.playing) { while (s.playing) { controller.tick(); delay(200) }; controller.tick() }
    LaunchedEffect(s.playing, controls, touched) { if (s.playing && controls) { delay(VIDEO_CONTROLS_MS); controls = false } }
    if (s.failed) {
        FileCard(m, FileCardState(appText("无法播放这个视频", "Can't play this video"), FileCardAction(appText("用其它应用打开", "Open in another app"), onOpenWith)), transition) {}
        return
    }
    val shown = controls || !s.playing
    BoxWithConstraints(Modifier.fillMaxSize().pointerInput(Unit) { detectTapGestures(onTap = { controls = !controls; touched++ }) }
        .semantics { contentDescription = m.name }) {
        val aspect = s.aspect ?: (16f / 9f)
        val fitWidth = minOf(maxWidth, maxHeight * aspect)
        Box(Modifier.align(Alignment.Center).size(fitWidth, fitWidth / aspect).then(transition)) { controller.Picture(Modifier.fillMaxSize()) }
        if (shown) {
            Box(Modifier.align(Alignment.Center).size(64.dp).clip(CircleShape).background(MediaColors.scrim)
                .press { if (s.playing) controller.pause() else controller.play(); touched++ }
                .semantics { contentDescription = if (s.playing) appText("暂停", "Pause") else appText("播放", "Play") },
                contentAlignment = Alignment.Center) {
                if (s.playing) Lucide(ViewerIcons.pauseFilled, 26.dp, MediaColors.icon)
                else Lucide(ViewerIcons.play, 26.dp, MediaColors.icon, modifier = Modifier.offset(x = 2.dp))
            }
            Row(Modifier.align(Alignment.BottomCenter).fillMaxWidth().then(transition).windowInsetsPadding(WindowInsets.navigationBars)
                .padding(start = 18.dp, end = 8.dp, bottom = CHROME_BOTTOM), verticalAlignment = Alignment.CenterVertically) {
                T(clock(s.positionMs), 12f, MediaColors.text, lineHeight = 16f, tabular = true)
                Scrubber(s, Modifier.weight(1f).padding(horizontal = 12.dp), onSeek = { controller.seekTo(it); touched++ })
                T(clock(s.durationMs), 12f, MediaColors.text2, lineHeight = 16f, tabular = true)
                ChromeButton(Modifier.padding(start = 4.dp), if (s.muted) ViewerIcons.muted else ViewerIcons.volume,
                    if (s.muted) appText("取消静音", "Unmute") else appText("静音", "Mute"), size = 22.dp) { controller.setMuted(!s.muted); touched++ }
            }
        }
    }
}

/** The video's timeline: a thin track, the played part and a knob; tap or drag to seek (it keeps the drag from paging). */
@Composable
private fun Scrubber(s: VideoState, modifier: Modifier, onSeek: (Long) -> Unit) {
    var dragging by remember { mutableStateOf<Float?>(null) }
    val fraction = dragging ?: if (s.durationMs > 0) (s.positionMs.toFloat() / s.durationMs).coerceIn(0f, 1f) else 0f
    fun seek(f: Float) { if (s.durationMs > 0) onSeek((f.coerceIn(0f, 1f) * s.durationMs).toLong()) }
    Canvas(modifier.height(40.dp)
        .pointerInput(s) { detectTapGestures(onTap = { seek(it.x / size.width) }) }
        .pointerInput(s) {
            detectHorizontalDragGestures(
                onDragStart = { dragging = (it.x / size.width).coerceIn(0f, 1f) },
                onDragEnd = { dragging?.let(::seek); dragging = null },
                onDragCancel = { dragging = null },
            ) { change, _ ->
                change.consume()
                dragging = (change.position.x / size.width).coerceIn(0f, 1f)
                dragging?.let(::seek)
            }
        }
        .semantics {
            contentDescription = appText("播放进度", "Playback position")
            progressBarRangeInfo = ProgressBarRangeInfo(fraction, 0f..1f)
            setProgress { seek(it); true }
        }) {
        val h = 3.dp.toPx(); val y = size.height / 2f
        drawRoundRect(MediaColors.track, Offset(0f, y - h / 2), Size(size.width, h), CornerRadius(h / 2))
        drawRoundRect(Color.White, Offset(0f, y - h / 2), Size(size.width * fraction, h), CornerRadius(h / 2))
        drawCircle(Color.White, (if (dragging != null) 8.dp else 6.dp).toPx(), Offset(size.width * fraction, y))
    }
}

/** m:ss, or h:mm:ss from an hour. */
internal fun clock(ms: Long): String {
    val t = ms.coerceAtLeast(0L) / 1000
    val h = t / 3600; val m = t / 60 % 60; val s = t % 60
    return if (h > 0) String.format(Locale.US, "%d:%02d:%02d", h, m, s) else String.format(Locale.US, "%d:%02d", m, s)
}
