package com.hachiwii.oink.ui

import android.os.Build
import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.animate
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.calculateCentroid
import androidx.compose.foundation.gestures.calculatePan
import androidx.compose.foundation.gestures.calculateZoom
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChanged
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.layout
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.lerp
import androidx.compose.ui.util.lerp
import androidx.compose.ui.zIndex
import java.util.Locale
import kotlin.math.max
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/*
 * The full-screen media viewer (native-rewrite.md 「全屏查看器」, Android, 用户 2026-11-11).
 * Left / right pages through every file of the transfer history in message order (a stack's items
 * one by one); folders and text are not in it. Opened from a folder listing, it pages through
 * that level's files instead. Images, videos (Media3) and PDFs (PdfRenderer)
 * show in place once on this phone; a picture under [AUTO_IMAGE_BYTES] is fetched as soon as its
 * page shows; everything else is a card with the file's icon, name and size and 下载 /
 * 用其它应用打开.
 */

/** Pictures smaller than this download by themselves when their page shows; larger ones wait for 下载. */
internal const val AUTO_IMAGE_BYTES = 16_000_000L

/** How long the save button shows its check mark (and ignores taps) after a tap. */
internal const val SAVE_CONFIRM_MS = 2000L

/** What the viewer can draw itself. */
internal enum class ViewerKind { Image, Video, Pdf }

/** --c-media-*: the viewer is always dark, whatever the app's theme. */
internal object MediaColors {
    val backdrop = Color.Black
    const val backdropAlpha = .96f
    val icon = Color(0xFFE6E8EE)
    val text = Color.White
    val text2 = Color(0xFFA7ADBA)
    val track = Color.White.copy(alpha = .18f)
    val button = Color.White.copy(alpha = .14f)
    val tile = Color.White.copy(alpha = .10f)
    val scrim = Color.Black.copy(alpha = .45f)
}

private val IMAGE_EXT = setOf("png", "jpg", "jpeg", "gif", "webp", "bmp", "heic", "heif") +
    (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) setOf("avif") else emptySet())
private val IMAGE_MIME = setOf("image/png", "image/jpeg", "image/jpg", "image/pjpeg", "image/gif", "image/webp", "image/bmp", "image/x-ms-bmp",
    "image/heic", "image/heif", "image/heic-sequence", "image/heif-sequence") +
    (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) setOf("image/avif") else emptySet())
private val VIDEO_EXT = setOf("mp4", "m4v", "mov", "3gp", "3g2", "webm", "mkv")
private val VIDEO_MIME = setOf("video/mp4", "video/x-m4v", "video/quicktime", "video/3gpp", "video/3gpp2", "video/webm", "video/x-matroska")

/**
 * What the viewer could draw for [m] once it is on this phone, from the record's MIME type and the
 * name's extension (ImageDecoder formats, incl. HEIC and, from Android 12, AVIF; containers
 * ExoPlayer reads; PDF); null for anything else. An image message with neither a type nor an
 * extension is taken as the image the core says it is.
 */
internal fun viewerKind(m: MessageUi): ViewerKind? {
    if (m.kind != "image" && m.kind != "file") return null
    val mime = m.mime?.substringBefore(';')?.trim()?.lowercase(Locale.US)?.takeIf { it.isNotEmpty() && it != "application/octet-stream" }
    val ext = m.name.substringAfterLast('.', "").lowercase(Locale.US)
    return when {
        mime in IMAGE_MIME || (mime == null && ext in IMAGE_EXT) -> ViewerKind.Image
        mime in VIDEO_MIME || (mime == null && ext in VIDEO_EXT) -> ViewerKind.Video
        mime == "application/pdf" || (mime == null && ext == "pdf") -> ViewerKind.Pdf
        m.kind == "image" && mime == null && ext.isEmpty() -> ViewerKind.Image
        else -> null
    }
}

/** Every file of the history pages in the viewer (folders and text do not). */
internal fun inViewer(m: MessageUi): Boolean = m.kind == "image" || m.kind == "file"

/**
 * The viewer's pages: the history's files in message order (a stack's items are separate
 * messages, so they come one by one in their order). An item that is not in the history (a
 * picture from a folder listing) pages alone.
 */
internal fun viewerSequence(messages: List<MessageUi>, current: Long, opened: MessageUi): List<MessageUi> =
    if (messages.none { it.id == current }) listOf(opened) else messages.filter(::inViewer)

/**
 * Where item [id] sits in its stack (用户 2026-11-11): its place among the stack's items the
 * viewer pages through and their number (consecutive messages of one batch; folders are not
 * counted, so the numbers run on without gaps). Null for a single message, for a stack that
 * leaves only one such item, and in a folder listing (no batches there).
 */
internal fun batchPosition(messages: List<MessageUi>, id: Long): Pair<Int, Int>? {
    val i = messages.indexOfFirst { it.id == id }.takeIf { it >= 0 } ?: return null
    val batch = messages[i].batch ?: return null
    var a = i; while (a > 0 && messages[a - 1].batch == batch) a--
    var b = i; while (b + 1 < messages.size && messages[b + 1].batch == batch) b++
    val paged = messages.subList(a, b + 1).filter(::inViewer)
    val at = paged.indexOfFirst { it.id == id }
    return if (paged.size > 1 && at >= 0) (at + 1) to paged.size else null
}

/** A folder listing entry's id as a viewer item: negative, so it never matches a history message (or its bubble). */
internal fun folderItemId(item: FolderItemUi): Long = -1L - item.entryId

/** A file of a folder listing as a viewer item; not downloaded yet counts as progress 0 (not the placeholder artwork). */
internal fun folderViewerItem(item: FolderItemUi) = MessageUi(folderItemId(item), false, "", "", "", true, item.kind, name = item.name, detail = item.detail,
    progress = item.progress ?: if (item.path == null) 0 else 100, imagePath = item.path?.takeIf { item.kind == "image" }, localPath = item.path,
    mime = item.mime, size = item.size, downloading = item.progress != null && item.progress < 100, downloadFailure = item.failure,
    thumbnailPath = item.thumbnailPath)

/** The viewer's pages for a folder listing (用户 2026-11-11): the files of that level in listing order; subfolders stay out. */
internal fun folderViewerItems(folder: FolderUi): List<MessageUi> = folder.items.filter { it.kind != "folder" }.map(::folderViewerItem)

/** How an item's page looks. */
internal enum class PageShow {
    /** The picture (or, for design scenes and own pictures without their file, the placeholder artwork). */
    Image,
    Video, Pdf,
    /** A small picture on its way: the PC's thumbnail with a small ring, or a ring alone. */
    ImageLoading,
    /** The file's icon, name and size with 下载 / 用其它应用打开 / why it cannot be fetched. */
    Card,
}

internal fun localFile(m: MessageUi): String? = m.imagePath ?: m.localPath

private fun expired(m: MessageUi) = m.unavailable == "expired" || m.preview == "expired"
private fun offline(m: MessageUi) = m.unavailable == "offline" || m.preview == "offline"

internal fun pageShow(m: MessageUi): PageShow {
    val kind = viewerKind(m)
    if (kind != null && localFile(m) != null) return when (kind) {
        ViewerKind.Image -> PageShow.Image
        ViewerKind.Video -> PageShow.Video
        ViewerKind.Pdf -> PageShow.Pdf
    }
    if (kind != ViewerKind.Image) return PageShow.Card
    // Nothing coming and nothing wrong: design scenes and own pictures whose file is gone.
    if (m.kind == "image" && (m.progress ?: 100) >= 100 && !m.downloading && m.downloadFailure == null && !expired(m) && !offline(m) && m.ended == null)
        return PageShow.Image
    val small = m.direct || m.size < AUTO_IMAGE_BYTES
    return if (!m.self && small && m.downloadFailure == null && !expired(m) && !offline(m) && m.ended == null) PageShow.ImageLoading else PageShow.Card
}

/** A page showing a small picture that is not here yet starts its download (once; never for one arriving by itself). */
internal fun fetchesOnShow(m: MessageUi): Boolean =
    pageShow(m) == PageShow.ImageLoading && !m.direct && !m.downloading && (m.progress ?: 0) < 100

/** The viewer's own zoom: [scale] ≥ 1 around the page centre plus [offset], kept so the content covers what it can. */
@Stable
internal class ZoomState {
    var scale by mutableFloatStateOf(1f)
    var offset by mutableStateOf(Offset.Zero)
    /** The page and the content's unscaled size in it (centred). */
    var viewport = Size.Zero
    var content = Size.Zero
    val zoomed by derivedStateOf { scale > 1.01f }

    private fun bounds(s: Float) = Offset(max(0f, (content.width * s - viewport.width) / 2f), max(0f, (content.height * s - viewport.height) / 2f))
    fun clamp(o: Offset, s: Float = scale): Offset { val b = bounds(s); return Offset(o.x.coerceIn(-b.x, b.x), o.y.coerceIn(-b.y, b.y)) }

    /**
     * Zooms by [factor] keeping [focus] (from the page centre) in place, then pans by [pan]. Returns
     * the vertical pan the bounds did not take (a PDF scrolls its pages by it).
     */
    fun transform(focus: Offset, factor: Float, pan: Offset): Float {
        val s = (scale * factor).coerceIn(1f, MAX_ZOOM)
        val wanted = focus - (focus - offset) * (s / scale) + pan
        val kept = clamp(wanted, s)
        scale = s
        offset = kept
        return wanted.y - kept.y
    }

    suspend fun animateTo(s: Float, o: Offset) {
        val s0 = scale; val o0 = offset
        animate(0f, 1f, animationSpec = tween(260, easing = FastOutSlowInEasing)) { t, _ ->
            scale = lerp(s0, s, t); offset = Offset(lerp(o0.x, o.x, t), lerp(o0.y, o.y, t))
        }
    }

    /** Double tap: back to fit when zoomed, else [DOUBLE_TAP_ZOOM] around the tapped point. */
    suspend fun toggle(focus: Offset) = if (zoomed) animateTo(1f, Offset.Zero) else animateTo(DOUBLE_TAP_ZOOM, clamp(focus * (1 - DOUBLE_TAP_ZOOM), DOUBLE_TAP_ZOOM))

    fun reset() { scale = 1f; offset = Offset.Zero }

    companion object {
        const val MAX_ZOOM = 4f
        const val DOUBLE_TAP_ZOOM = 2.5f
    }
}

/**
 * Pinch zoom, and one-finger panning while zoomed. It looks at events before the content
 * (a PDF's page list) and the pager do, and takes them only for a pinch or a zoomed pan, so a
 * one-finger swipe at fit size still pages (and scrolls a PDF). [overflowY] gets the vertical pan
 * the zoom bounds did not take.
 */
internal fun Modifier.zoomGestures(state: ZoomState, overflowY: ((Float) -> Unit)? = null): Modifier = pointerInput(state) {
    val slop = viewConfiguration.touchSlop
    awaitEachGesture {
        awaitFirstDown(requireUnconsumed = false, pass = PointerEventPass.Initial)
        var taking = false
        var moved = Offset.Zero
        do {
            val event = awaitPointerEvent(PointerEventPass.Initial)
            val fingers = event.changes.count { it.pressed }
            // The lifting event has no finger down (and no centroid): nothing to move.
            if (fingers >= 2 || (fingers == 1 && state.zoomed)) {
                val pan = event.calculatePan()
                if (!taking) { moved += pan; taking = fingers >= 2 || moved.getDistance() > slop }
                if (taking) {
                    val centre = Offset(size.width / 2f, size.height / 2f)
                    val rest = state.transform(event.calculateCentroid(useCurrent = true) - centre, event.calculateZoom(), pan)
                    if (rest != 0f) overflowY?.invoke(rest)
                    event.changes.forEach { if (it.positionChanged()) it.consume() }
                }
            }
        } while (event.changes.any { it.pressed })
    }
}

/**
 * The viewer, opened on [opened]: it grows out of the picture's bubble ([origin], root
 * coordinates) while the backdrop fades in and shrinks back into the bubble of the item shown when
 * it closes (fades when that one is no picture or not on screen). [onPage] follows the shown item;
 * [onClosed] runs when the closing animation ends. New messages join the pages without moving
 * the one shown (pages are keyed by message id).
 */
@Composable
internal fun MediaViewer(opened: MessageUi, messages: List<MessageUi>, origin: (Long) -> Rect?, onPage: (Long) -> Unit, onClosed: () -> Unit,
                         actions: AppActions, download: (MessageUi) -> Unit = actions.download) {
    val progress = remember { Animatable(0f) }
    val scope = rememberCoroutineScope()
    var currentId by remember { mutableLongStateOf(opened.id) }
    val pages = remember(messages, currentId) { viewerSequence(messages, currentId, opened) }
    val latestPages by rememberUpdatedState(pages)
    val pager = rememberPagerState(initialPage = pages.indexOfFirst { it.id == opened.id }.coerceAtLeast(0)) { latestPages.size }
    val pictureLike = { m: MessageUi -> pageShow(m).let { it == PageShow.Image || it == PageShow.ImageLoading } }
    val start = remember { if (pictureLike(opened)) origin(opened.id) else null }
    var closing by remember { mutableStateOf(false) }
    var back by remember { mutableStateOf<Rect?>(null) }
    val zooms = remember { HashMap<Long, ZoomState>() }
    val asked = remember { HashSet<Long>() }
    fun zoom(id: Long) = zooms.getOrPut(id) { ZoomState() }
    LaunchedEffect(Unit) { progress.animateTo(1f, tween(320, easing = EaseOutExpo)) }
    LaunchedEffect(pager) {
        snapshotFlow { pager.currentPage }.collect { i ->
            val id = latestPages.getOrNull(i)?.id ?: return@collect
            if (id != currentId) { currentId = id; onPage(id) }
        }
    }
    // A page left behind is back at fit size when it comes back.
    LaunchedEffect(pager.settledPage) { val shown = latestPages.getOrNull(pager.settledPage)?.id; zooms.forEach { (id, z) -> if (id != shown) z.reset() } }
    fun close() {
        if (closing) return
        closing = true
        val shown = latestPages.getOrNull(pager.currentPage)
        back = shown?.takeIf(pictureLike)?.let { origin(it.id) }
        scope.launch { progress.animateTo(0f, tween(260, easing = FastOutSlowInEasing)); onClosed() }
    }
    BackHandler { close() }
    val p = { progress.value }
    Box(Modifier.fillMaxSize().zIndex(10f).drawBehind { drawRect(MediaColors.backdrop.copy(alpha = MediaColors.backdropAlpha * progress.value)) }) {
        val current = pages.getOrNull(pager.currentPage)
        HorizontalPager(pager, Modifier.fillMaxSize(), pageSpacing = 16.dp, key = { latestPages[it].id },
            userScrollEnabled = !closing && current?.let { zoom(it.id).zoomed } != true) { page ->
            val m = pages[page]
            val isCurrent = page == pager.currentPage
            val from = when {
                !isCurrent -> null
                closing -> back
                m.id == opened.id -> start
                else -> null
            }
            val settled = page == pager.settledPage
            // A small picture that is not here yet is fetched when its page shows (once; paging on keeps it going).
            LaunchedEffect(settled, m.id, fetchesOnShow(m)) { if (settled && fetchesOnShow(m) && asked.add(m.id)) download(m) }
            ViewerPage(m, isCurrent = isCurrent, settled = settled, closing = closing,
                progress = if (isCurrent) p else ({ 1f }), from = from, zoom = zoom(m.id), actions = actions, download = download, onClose = ::close)
        }
        // Chrome: close at the top (and, for an item of a stack, its place in the stack); save,
        // open in another app and share at the bottom (white on dark).
        Box(Modifier.fillMaxWidth().alpha(progress.value).windowInsetsPadding(WindowInsets.statusBars)) {
            Box(Modifier.padding(start = 14.dp, top = 14.dp).press { close() }.semantics { contentDescription = "关闭" }) {
                T("×", 30f, Color.White, lineHeight = 34f)
            }
            current?.let { batchPosition(messages, it.id) }?.let { (at, of) ->
                T("$at / $of", 13f, MediaColors.text2, Modifier.align(Alignment.TopCenter).padding(top = 22.dp), lineHeight = 18f, tabular = true)
            }
        }
        if (current != null) {
            val local = localFile(current)
            val show = pageShow(current)
            Box(Modifier.fillMaxWidth().align(Alignment.BottomCenter).alpha(progress.value).windowInsetsPadding(WindowInsets.navigationBars)
                .padding(horizontal = 12.dp, vertical = 12.dp).height(48.dp)) {
                if (local != null && (show == PageShow.Image || show == PageShow.Video))
                    key(current.id) { SaveButton(Modifier.align(Alignment.CenterStart)) { actions.saveImage(current) } }
                if (local != null) Row(Modifier.align(Alignment.CenterEnd), horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                    ChromeButton(Modifier, DesignIconData.lucideExternalLink, appText("用其它应用打开", "Open in another app")) { actions.openWith(current) }
                    ChromeButton(Modifier, ViewerIcons.share, appText("分享", "Share")) { actions.shareFile(current) }
                }
            }
        }
    }
}

/** Height of the bottom chrome row (save / share) with its margins, above the navigation bar. */
internal val CHROME_BOTTOM = 72.dp

@Composable
private fun ViewerPage(m: MessageUi, isCurrent: Boolean, settled: Boolean, closing: Boolean, progress: () -> Float, from: Rect?, zoom: ZoomState,
                       actions: AppActions, download: (MessageUi) -> Unit, onClose: () -> Unit) {
    // Pages other than pictures fade and scale from the centre.
    val fade = Modifier.graphicsLayer { val t = progress(); alpha = t; scaleX = lerp(.85f, 1f, t); scaleY = lerp(.85f, 1f, t) }
    when (pageShow(m)) {
        PageShow.Image -> ImagePage(m, progress, from, zoom, actions, onClose)
        PageShow.ImageLoading -> ImagePage(m, progress, from, zoom, actions, onClose)
        PageShow.Video -> VideoPage(m, localFile(m)!!, isCurrent, settled, closing, fade, onOpenWith = { actions.openWith(m) })
        PageShow.Pdf -> PdfPage(m, localFile(m)!!, zoom, progress, onOpenWith = { actions.openWith(m) })
        PageShow.Card -> FileCard(m, cardState(m, download, actions.openWith), fade, onClose)
    }
}

/** Decoding result of the viewer's full-size picture. */
private sealed interface Decoded { data object Loading : Decoded; data class Done(val image: ImageBitmap) : Decoded; data object Failed : Decoded }

@Composable
private fun rememberDecoded(path: String?, maxPx: Int): Decoded {
    val state by produceState<Decoded>(path?.let { cachedImage(it, maxPx) }?.let { Decoded.Done(it) } ?: Decoded.Loading, path) {
        if (path == null) return@produceState
        cachedImage(path, maxPx)?.let { value = Decoded.Done(it); return@produceState }
        value = withContext(Dispatchers.IO) {
            runCatching {
                val source = android.graphics.ImageDecoder.createSource(java.io.File(path))
                android.graphics.ImageDecoder.decodeBitmap(source) { decoder, info, _ ->
                    val long = maxOf(info.size.width, info.size.height)
                    if (long > maxPx) decoder.setTargetSampleSize(long / maxPx)
                    decoder.allocator = android.graphics.ImageDecoder.ALLOCATOR_SOFTWARE
                }.asImageBitmap()
            }.getOrNull()
        }?.also { cacheImage(path, maxPx, it) }?.let { Decoded.Done(it) } ?: Decoded.Failed
    }
    return state
}

/**
 * A picture page: the bubble's decoded picture at once, the full-size one when ready; while a
 * small picture downloads, the PC's thumbnail with a small ring (or a ring alone). Pinch and
 * double tap zoom; a single tap closes the viewer.
 */
@Composable
private fun ImagePage(m: MessageUi, progress: () -> Float, from: Rect?, zoom: ZoomState, actions: AppActions, onClose: () -> Unit) {
    val path = localFile(m)
    val quick = rememberImage(path, 720)
    val full = rememberDecoded(path, 2048)
    if (path != null && full == Decoded.Failed && quick == null) {
        FileCard(m, FileCardState(appText("无法显示这张图片", "Can't show this image"), FileCardAction(appText("用其它应用打开", "Open in another app")) { actions.openWith(m) }),
            Modifier, onClose)
        return
    }
    val image = (full as? Decoded.Done)?.image ?: quick
    val thumbnail = rememberImage(m.thumbnailPath.takeIf { image == null }, 720)
    val scope = rememberCoroutineScope()
    val zoomable = image != null
    BoxWithConstraints(Modifier.fillMaxSize()
        .then(if (zoomable) Modifier.zoomGestures(zoom) else Modifier)
        .pointerInput(zoomable) {
            val toggle: (Offset) -> Unit = { at -> scope.launch { zoom.toggle(at - Offset(size.width / 2f, size.height / 2f)) } }
            detectTapGestures(onDoubleTap = if (zoomable) toggle else null, onTap = { onClose() })
        }) {
        val width = constraints.maxWidth.toFloat()
        val height = constraints.maxHeight.toFloat()
        val aspect = (image ?: thumbnail)?.let { it.width.toFloat() / it.height.coerceAtLeast(1) } ?: m.aspect ?: (260f / 146f)
        val fitWidth = minOf(width, height * aspect)
        val target = Rect(Offset((width - fitWidth) / 2f, (height - fitWidth / aspect) / 2f), Size(fitWidth, fitWidth / aspect))
        zoom.viewport = Size(width, height); zoom.content = target.size
        Box(Modifier.layout { measurable, constraints ->
            val t = progress()
            val r = if (from != null) lerpRect(from, target, t) else lerpRect(scaleAround(target, .85f), target, t)
            val placeable = measurable.measure(Constraints.fixed(r.width.roundToInt().coerceAtLeast(1), r.height.roundToInt().coerceAtLeast(1)))
            layout(constraints.maxWidth, constraints.maxHeight) { placeable.place(r.left.roundToInt(), r.top.roundToInt()) }
        }.graphicsLayer {
            // Zoom eases out with the closing animation, so the picture lands on its bubble.
            val t = progress()
            scaleX = lerp(1f, zoom.scale, t); scaleY = scaleX
            translationX = zoom.offset.x * t; translationY = zoom.offset.y * t
        }.graphicsLayer {
            val t = progress()
            alpha = if (from == null) t else 1f
            shape = RoundedCornerShape(lerp(18.dp, 0.dp, t))
            clip = true
        }.semantics { contentDescription = m.name }, contentAlignment = Alignment.Center) {
            when {
                image != null -> Image(image, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
                thumbnail != null -> {
                    Image(thumbnail, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
                    Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = .28f)))
                    ProgressRing(m.progress ?: 0, 36.dp, numberColor = Color.White, track = Color.White.copy(alpha = .3f))
                }
                pageShow(m) == PageShow.ImageLoading -> ProgressRing(m.progress ?: 0, 56.dp, numberColor = MediaColors.text2, track = MediaColors.track)
                path != null -> {}
                else -> FullVector(DesignIconData.previewImage)
            }
        }
    }
}

/** A card's status line and button (or [ring] progress instead of the button). */
internal class FileCardState(val status: String? = null, val button: FileCardAction? = null, val ring: Int? = null)
internal class FileCardAction(val label: String, val onClick: () -> Unit)

/** What a file's card says and offers. */
internal fun cardState(m: MessageUi, download: (MessageUi) -> Unit, openWith: (MessageUi) -> Unit): FileCardState {
    val fetch = FileCardAction(appText("下载", "Download")) { download(m) }
    return when {
        localFile(m) != null -> FileCardState(button = FileCardAction(appText("用其它应用打开", "Open in another app")) { openWith(m) })
        m.ended == "failed" -> FileCardState(appText("接收失败", "Not received"))
        m.ended == "cancelled" -> FileCardState(appText("已取消", "Cancelled"))
        m.downloading || (m.direct && !m.self) -> FileCardState(ring = m.progress?.coerceIn(0, 99) ?: 0)
        m.self -> FileCardState(appText("文件已不在这台手机上", "The file is no longer on this phone"))
        m.downloadFailure != null -> FileCardState(m.downloadFailure, FileCardAction(appText("重试", "Retry")) { download(m) })
        expired(m) -> FileCardState(appText("内容已失效", "No longer available"))
        offline(m) -> FileCardState(appText("来源设备离线，暂时无法获取", "The source device is offline"), fetch)
        else -> FileCardState(button = fetch)
    }
}

/** A file's page: its type icon, name and size, why it cannot come (if so) and its button or progress. */
@Composable
internal fun FileCard(m: MessageUi, state: FileCardState, modifier: Modifier, onClose: () -> Unit) {
    val glyph = when (viewerKind(m)) {
        ViewerKind.Image -> DesignIconData.lucideImage
        ViewerKind.Video -> DesignIconData.lucidePlay
        ViewerKind.Pdf -> DesignIconData.lucideFileText
        null -> fileGlyph(m)
    }
    Box(Modifier.fillMaxSize().pointerInput(Unit) { detectTapGestures(onTap = { onClose() }) }.then(modifier), contentAlignment = Alignment.Center) {
        Column(Modifier.padding(horizontal = 32.dp).widthIn(max = 360.dp), horizontalAlignment = Alignment.CenterHorizontally) {
            Box(Modifier.size(72.dp, 80.dp).clip(RoundedCornerShape(18.dp)).background(MediaColors.tile), contentAlignment = Alignment.Center) {
                Lucide(glyph, 34.dp, MediaColors.icon, 1.6f)
            }
            T(m.name, 16f, MediaColors.text, Modifier.padding(top = 18.dp), weight = 600, lineHeight = 22.4f, align = TextAlign.Center, maxLines = 3, ellipsis = true)
            if (m.detail.isNotEmpty()) T(m.detail, 12f, MediaColors.text2, Modifier.padding(top = 6.dp), lineHeight = 16.8f, align = TextAlign.Center, tabular = true)
            state.status?.let { T(it, 13f, MediaColors.text2, Modifier.padding(top = 18.dp), lineHeight = 18.2f, align = TextAlign.Center) }
            Box(Modifier.padding(top = 22.dp).heightIn(min = 48.dp), contentAlignment = Alignment.Center) {
                when {
                    state.ring != null -> ProgressRing(state.ring, 48.dp, numberColor = MediaColors.text2, track = MediaColors.track)
                    state.button != null -> Box(Modifier.heightIn(min = 44.dp).clip(CircleShape).background(MediaColors.button)
                        .press(onClick = state.button.onClick).padding(horizontal = 22.dp, vertical = 11.dp), contentAlignment = Alignment.Center) {
                        T(state.button.label, 14f, MediaColors.text, weight = 600, lineHeight = 20f)
                    }
                }
            }
        }
    }
}

/** A bare light icon button of the viewer's chrome (48 target). */
@Composable
internal fun ChromeButton(modifier: Modifier, icon: IconData, label: String, size: Dp = 24.dp, onClick: () -> Unit) {
    Box(modifier.size(48.dp).press(onClick = onClick).semantics { contentDescription = label }, contentAlignment = Alignment.Center) {
        Lucide(icon, size, MediaColors.icon)
    }
}

/**
 * 保存到相册 (pictures and videos): a bare light icon at the bottom left. A tap turns it into a
 * check mark for [SAVE_CONFIRM_MS], during which further taps do nothing; the result toast shows
 * above the viewer.
 */
@Composable
private fun SaveButton(modifier: Modifier, onSave: () -> Unit) {
    var saved by remember { mutableStateOf(false) }
    LaunchedEffect(saved) { if (saved) { delay(SAVE_CONFIRM_MS); saved = false } }
    Box(modifier.size(48.dp).press { if (!saved) { saved = true; onSave() } }
        .semantics { contentDescription = if (saved) appText("已保存到相册", "Saved to Photos") else appText("保存到相册", "Save to Photos") },
        contentAlignment = Alignment.Center) {
        Lucide(if (saved) DesignIconData.lucideCheck else DesignIconData.lucideDownload, 24.dp, MediaColors.icon)
    }
}

internal fun lerpRect(a: Rect, b: Rect, t: Float) = Rect(lerp(a.left, b.left, t), lerp(a.top, b.top, t), lerp(a.right, b.right, t), lerp(a.bottom, b.bottom, t))
internal fun scaleAround(r: Rect, s: Float) = Rect(r.center.x - r.width * s / 2f, r.center.y - r.height * s / 2f, r.center.x + r.width * s / 2f, r.center.y + r.height * s / 2f)

/** Lucide icons the design export does not carry (lucide 0.4xx paths, 24 viewBox, stroke). */
internal object ViewerIcons {
    val pause = IconData(0f, 0f, 24f, 24f, listOf(
        IconPart("M15 4h2a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1h-2a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1z", 'S', 0f, 0),
        IconPart("M7 4h2a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1z", 'S', 0f, 0)))
    private const val SPEAKER = "M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z"
    val volume = IconData(0f, 0f, 24f, 24f, listOf(
        IconPart(SPEAKER, 'S', 0f, 0), IconPart("M16 9a5 5 0 0 1 0 6", 'S', 0f, 0), IconPart("M19.364 18.364a9 9 0 0 0 0-12.728", 'S', 0f, 0)))
    val muted = IconData(0f, 0f, 24f, 24f, listOf(
        IconPart(SPEAKER, 'S', 0f, 0), IconPart("M22 9l-6 6", 'S', 0f, 0), IconPart("M16 9l6 6", 'S', 0f, 0)))
    val share = IconData(0f, 0f, 24f, 24f, listOf(
        IconPart("M15 5a3 3 0 1 0 6 0a3 3 0 1 0 -6 0", 'S', 0f, 0),
        IconPart("M3 12a3 3 0 1 0 6 0a3 3 0 1 0 -6 0", 'S', 0f, 0),
        IconPart("M15 19a3 3 0 1 0 6 0a3 3 0 1 0 -6 0", 'S', 0f, 0),
        IconPart("M8.59 13.51l6.83 3.98", 'S', 0f, 0),
        IconPart("M15.41 6.51l-6.82 3.98", 'S', 0f, 0)))
    val play = IconData(0f, 0f, 24f, 24f, listOf(IconPart("M6 3L20 12L6 21L6 3Z", 'F', 0f, 0)))
    val pauseFilled = IconData(0f, 0f, 24f, 24f, listOf(
        IconPart("M15 4h2a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1h-2a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1z", 'F', 0f, 0),
        IconPart("M7 4h2a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1z", 'F', 0f, 0)))
}
