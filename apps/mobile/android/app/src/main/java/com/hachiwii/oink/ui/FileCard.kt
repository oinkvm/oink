package com.hachiwii.oink.ui

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.media.ThumbnailUtils
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import java.io.File
import java.security.MessageDigest
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/*
 * File cards (native-rewrite.md 「文件卡片」, file-bubble SPEC part 5, Android 1–5 and 10–11): single
 * file / folder messages, the items of an expanded batch and the folder page's rows share the stack
 * card's language — type tile, name, extension + size — and one right-hand button component.
 */

/** What the right-hand button of a received file shows. */
internal enum class FileSlot {
    /** Not downloaded: 32 round chip with the download arrow. */
    Download,
    /** Downloading, or arriving straight from a phone: the ring with its number. */
    Progress,
    /** Downloaded: a check. */
    Done,
    /** The last download failed and can be tried again: red retry chip. */
    Retry,
    /** A phone-to-phone receive failed: red alert, nothing to retry (the sender is gone). */
    Failed,
    /** Its computer is offline: the download arrow without chip, faint, not tappable. */
    Offline,
}

/**
 * A file card's state. [reason] replaces the size on the second line (red when [reasonBad]); [grey]
 * greys the type tile (cancelled, failed phone-to-phone receive, expired); [inert] cards do nothing
 * when tapped (内容已失效).
 */
internal data class FileLook(val slot: FileSlot?, val progress: Int = 0, val reason: String? = null, val reasonBad: Boolean = false,
                             val grey: Boolean = false, val inert: Boolean = false)

private fun expiredText() = appText("已失效", "Expired")
private fun offlineText() = appText("暂时无法加载", "Unavailable for now")
private fun downloadFailedText() = appText("下载失败", "Download failed")

/**
 * The card state of a message (file-bubble SPEC 1.3 / 1.4 / 4.1). Own files and folders have no
 * button, and an own failed send keeps its colours (the status line says why). Received files: the
 * download chip, its ring, a check, the retry chip after a failed download, or — straight from a
 * phone — the ring while it arrives and the red alert when that failed. Received folders only say
 * why they cannot be opened; one arriving straight from a phone shows its ring meanwhile.
 */
internal fun fileLook(m: MessageUi): FileLook {
    val folder = m.kind == "folder"
    return when {
        m.self -> FileLook(null)
        m.ended == "cancelled" -> FileLook(null, grey = true)
        m.ended == "failed" -> FileLook(if (folder) null else FileSlot.Failed, grey = true)
        m.direct -> when {
            (m.progress ?: 0) < 100 -> FileLook(FileSlot.Progress, m.progress ?: 0)
            folder -> FileLook(null)
            else -> FileLook(FileSlot.Done)
        }
        m.unavailable == "expired" -> FileLook(null, reason = expiredText(), grey = true, inert = true)
        folder -> FileLook(null, reason = offlineText().takeIf { m.unavailable == "offline" })
        m.kind != "file" -> FileLook(null)
        m.progress == 100 -> FileLook(FileSlot.Done)
        m.progress != null -> FileLook(FileSlot.Progress, m.progress)
        m.unavailable == "offline" -> FileLook(FileSlot.Offline, reason = offlineText())
        m.downloadFailure != null -> FileLook(FileSlot.Retry, reason = downloadFailedText(), reasonBad = true)
        else -> FileLook(FileSlot.Download)
    }
}

/** The card state of a folder page row: subfolders have none (a chevron instead); files and pictures as received files. */
internal fun fileLook(item: FolderItemUi): FileLook = when {
    item.kind == "folder" -> FileLook(null)
    item.progress == 100 -> FileLook(FileSlot.Done)
    item.progress != null -> FileLook(FileSlot.Progress, item.progress)
    item.failure != null -> FileLook(FileSlot.Retry, reason = downloadFailedText(), reasonBad = true)
    else -> FileLook(FileSlot.Download)
}

/** The size part of a message's detail ("PDF · 2.4 MB" → "2.4 MB"); a folder's 「N 个文件」 as it is. */
internal fun cardSize(m: MessageUi): String =
    if (m.kind == "folder") m.detail else m.detail.split(" · ").filter { it.isNotEmpty() && it != fileExt(m) }.joinToString(" · ")

/** Type tile (36 × 36, radius 10): the type colour 16 % (folders 18 %) over the card, icon 20; grey when the content is gone. */
@Composable
internal fun FileTypeTile(folder: Boolean, glyph: IconData, grey: Boolean = false) {
    val c = Oink.colors
    val tint = when { grey -> c.stackCardHint; folder -> c.folder; else -> c.file }
    Box(Modifier.size(36.dp).clip(RoundedCornerShape(10.dp)).background(OinkColors.mix(tint, c.stackCard, if (folder && !grey) .18f else .16f)),
        contentAlignment = Alignment.Center) {
        Lucide(glyph, 20.dp, tint, 1.8f)
    }
}

/** Name (at most three lines) over the extension and the size or [reason]. Chat cards 12 / 10, folder rows 14 / 11. */
@Composable
internal fun FileCardText(name: String, ext: String, size: String, reason: String?, reasonBad: Boolean, modifier: Modifier = Modifier, row: Boolean = false) {
    val c = Oink.colors
    Column(modifier, verticalArrangement = Arrangement.spacedBy(4.dp)) {
        if (row) T(name, 14f, c.stackCardInk, weight = 600, lineHeight = 18.9f, maxLines = 3, ellipsis = true)
        else T(name, 12f, c.stackCardInk, weight = 600, lineHeight = 15.6f, maxLines = 3, ellipsis = true)
        val second = reason ?: size
        if (ext.isNotEmpty() || second.isNotEmpty()) Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            if (ext.isNotEmpty()) T(ext, 9f, c.stackCardHint, Modifier.alignByBaseline(), weight = 650, lineHeight = 12f, letterSpacing = .4f)
            if (second.isNotEmpty()) T(second, if (row) 11f else 10f, if (reasonBad) c.badInk else c.stackCardHint, Modifier.alignByBaseline(),
                lineHeight = if (row) 15f else 12f, ellipsis = true, tabular = true)
        }
    }
}

/**
 * The right-hand button (40 × 44 touch area) shared by single file cards, expanded batch items and
 * folder rows. Only 下载 and 重试 take taps; otherwise the tap reaches the card (opens the item).
 */
@Composable
internal fun FileSlotButton(state: FileLook, name: String, onClick: (() -> Unit)?) {
    val c = Oink.colors
    val slot = state.slot ?: return
    val tappable = onClick != null && (slot == FileSlot.Download || slot == FileSlot.Retry)
    Box(Modifier.size(40.dp, 44.dp).then(if (tappable) Modifier.press { onClick!!() }.semantics {
        contentDescription = if (slot == FileSlot.Retry) appText("重试下载 $name", "Retry download $name") else appText("下载 $name", "Download $name")
    } else Modifier), contentAlignment = Alignment.Center) {
        when (slot) {
            FileSlot.Download -> Box(Modifier.size(32.dp).clip(CircleShape).background(OinkColors.mix(c.stackCardInk, c.stackCard, .06f)), contentAlignment = Alignment.Center) {
                Lucide(DesignIconData.lucideDownload, 16.dp, c.text2)
            }
            FileSlot.Offline -> Lucide(DesignIconData.lucideDownload, 16.dp, c.faint.copy(alpha = .6f))
            FileSlot.Retry -> Box(Modifier.size(32.dp).clip(CircleShape).background(OinkColors.mix(c.bad, c.stackCard, .12f)), contentAlignment = Alignment.Center) {
                Lucide(DesignIconData.lucideRotateCw, 15.dp, c.badInk)
            }
            // On a card the usual track would vanish in dark mode (it is the card's colour).
            FileSlot.Progress -> ProgressRing(state.progress, 36.dp, track = OinkColors.mix(c.stackCardInk, c.stackCard, .12f))
            FileSlot.Done -> Lucide(DesignIconData.lucideCheck, 18.dp, c.stackCardHint)
            FileSlot.Failed -> Lucide(DesignIconData.lucideAlertCircle, 20.dp, c.bad)
        }
    }
}

/**
 * A file / folder message's card content: tile, text and the button, padded 12 (right 8 with a
 * button, 14 without), the button against the right edge. No surface: [FileMessageCard] adds it, the
 * expand / collapse morph draws its own. Without handlers it is a still picture (the morph's stand-in).
 */
@Composable
internal fun FileCardRow(m: MessageUi, modifier: Modifier, onDownload: (() -> Unit)?) {
    val state = fileLook(m)
    val button = state.slot != null
    Row(modifier.heightIn(min = 70.dp).padding(start = 12.dp, end = if (button) 8.dp else 14.dp, top = 12.dp, bottom = 12.dp),
        horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
        Row(Modifier.weight(1f, fill = false).padding(end = if (button) 10.dp else 0.dp), horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalAlignment = Alignment.CenterVertically) {
            FileTypeTile(m.kind == "folder", fileGlyph(m), state.grey)
            FileCardText(m.name, fileExt(m), cardSize(m), state.reason, state.reasonBad)
        }
        FileSlotButton(state, m.name, onDownload)
    }
}

private val CardShape = RoundedCornerShape(12.dp)

/**
 * A file / folder message as a card (white / #2D323E, radius 12, 1 dp inner edge, no shadow), the
 * same for both sides. A tap opens the item (nothing for expired content), a long press anywhere
 * on it opens the message menu.
 */
@Composable
internal fun FileMessageCard(m: MessageUi, modifier: Modifier, onOpen: () -> Unit, onDownload: () -> Unit, onLongPress: (Offset) -> Unit) {
    val c = Oink.colors
    val inert = fileLook(m).inert
    FileCardRow(m, modifier.clip(CardShape).background(c.stackCard).border(1.dp, c.stackCardEdge, CardShape)
        .pressLong(onClick = { if (!inert) onOpen() }, onLongClick = onLongPress), onDownload)
}

// ---- folder page thumbnails ----

/**
 * The 36 × 36 leading square of a folder row: a picture's thumbnail (cover, 1 dp inner edge) once
 * there is one — made here from the file when it is on this phone (downloaded, or the folder came
 * straight from a phone), else the computer's thumbnail — otherwise the type tile.
 */
@Composable
internal fun FolderRowLeading(item: FolderItemUi) {
    val c = Oink.colors
    val image = item.kind == "image"
    val local = rememberLocalThumbnail(item.path.takeIf { image })
    val remote = rememberImage(item.thumbnailPath.takeIf { image && local == null }, 256)
    val picture = local ?: remote
    if (picture != null) {
        val shape = RoundedCornerShape(10.dp)
        Box(Modifier.size(36.dp).clip(shape).mediaEdge(c, shape)) {
            Image(picture, null, Modifier.matchParentSize(), contentScale = ContentScale.Crop)
        }
    } else FileTypeTile(item.kind == "folder", fileGlyph(item.kind, item.name))
}

/** Thumbnails made from local pictures, in the app's cache (`cacheDir/folder-thumbnails`), keyed by path, size and change time. */
internal object LocalThumbnails {
    private const val DIR = "folder-thumbnails"

    fun memoryKey(path: String) = "thumb:$path"

    private fun cacheName(file: File): String {
        val digest = MessageDigest.getInstance("SHA-1").digest("${file.absolutePath}:${file.length()}:${file.lastModified()}".toByteArray())
        return digest.joinToString("") { "%02x".format(it) } + ".jpg"
    }

    /** The cached thumbnail, or a new one ([px] on its short side or more, orientation applied); null when the file cannot be read as a picture. */
    fun load(cacheDir: File, path: String, px: Int): ImageBitmap? {
        val file = File(path)
        if (!file.isFile) return null
        val cached = File(File(cacheDir, DIR), cacheName(file))
        if (cached.isFile) BitmapFactory.decodeFile(cached.path)?.let { return it.asImageBitmap() }
        val bitmap = runCatching { ThumbnailUtils.createImageThumbnail(file, android.util.Size(px, px), null) }.getOrNull() ?: return null
        runCatching {
            cached.parentFile?.mkdirs()
            val partial = File(cached.parentFile, cached.name + ".part")
            partial.outputStream().use { bitmap.compress(Bitmap.CompressFormat.JPEG, 85, it) }
            partial.renameTo(cached)
        }
        return bitmap.asImageBitmap()
    }
}

/** A local picture's small thumbnail (folder rows), from memory, the cache directory, or made off the main thread. */
@Composable
internal fun rememberLocalThumbnail(path: String?, size: Dp = 72.dp): ImageBitmap? {
    val context = LocalContext.current
    val px = with(LocalDensity.current) { size.toPx().roundToInt() }
    val key = path?.let(LocalThumbnails::memoryKey)
    val image by produceState(key?.let { ImageCache.get(it) }, key) {
        if (path == null || key == null) { value = null; return@produceState }
        ImageCache.get(key)?.let { value = it; return@produceState }
        value = withContext(Dispatchers.IO) { LocalThumbnails.load(context.cacheDir, path, px) }?.also { ImageCache.put(key, it) }
    }
    return image
}
