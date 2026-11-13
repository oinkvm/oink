package com.hachiwii.oink.ui

import android.graphics.Bitmap
import android.graphics.pdf.PdfRenderer
import android.os.ParcelFileDescriptor
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.compose.ui.util.lerp
import java.io.Closeable
import java.io.File
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** An open PDF: its pages' sizes (points) and their pictures. Tests stand in a fake through [LocalPdfOpener]. */
internal interface PdfDocument : Closeable {
    val pageSizes: List<Size>
    /** Draws page [index] [widthPx] wide on white. Called on [pdfThread] only. */
    fun render(index: Int, widthPx: Int): ImageBitmap
}

/** Opens a PDF (on [pdfThread]); android.graphics.pdf.PdfRenderer outside tests. */
internal val LocalPdfOpener = staticCompositionLocalOf<(String) -> PdfDocument> { { path -> RendererPdf(path) } }

/** PdfRenderer allows one open page at a time and is not thread-safe: every document call runs here, in order. */
@OptIn(kotlinx.coroutines.ExperimentalCoroutinesApi::class)
internal val pdfThread = Dispatchers.IO.limitedParallelism(1)
private val pdfScope = CoroutineScope(SupervisorJob() + pdfThread)

private class RendererPdf(path: String) : PdfDocument {
    private val file = ParcelFileDescriptor.open(File(path), ParcelFileDescriptor.MODE_READ_ONLY)
    private val renderer = try { PdfRenderer(file) } catch (e: Exception) { file.close(); throw e }
    override val pageSizes = (0 until renderer.pageCount).map { i -> renderer.openPage(i).use { Size(it.width.toFloat(), it.height.toFloat()) } }
    override fun render(index: Int, widthPx: Int): ImageBitmap = renderer.openPage(index).use { page ->
        val height = (widthPx.toFloat() * page.height / page.width).toInt().coerceAtLeast(1)
        val bitmap = Bitmap.createBitmap(widthPx, height, Bitmap.Config.ARGB_8888)
        bitmap.eraseColor(android.graphics.Color.WHITE)
        page.render(bitmap, null, null, PdfRenderer.Page.RENDER_MODE_FOR_DISPLAY)
        bitmap.asImageBitmap()
    }
    override fun close() { renderer.close(); file.close() }
}

/** Rendered pages by "path:page:width"; scrolling back shows them at once. */
private object PdfPageCache : android.util.LruCache<String, ImageBitmap>(48 * 1024 * 1024) {
    override fun sizeOf(key: String, value: ImageBitmap) = value.width * value.height * 4
}

/**
 * A PDF page of the viewer: its pages one under another, the page's width, scrolled vertically.
 * Pinch and double tap zoom the whole column; zoomed in, one finger pans and scrolls the pages.
 * Pages are drawn at the screen's width, and again at twice it once zoomed past 1.6×. A PDF that
 * cannot be opened (damaged, password) becomes a card with 用其它应用打开.
 */
@Composable
internal fun PdfPage(m: MessageUi, path: String, zoom: ZoomState, progress: () -> Float, onOpenWith: () -> Unit) {
    val opener = LocalPdfOpener.current
    val doc by produceState<Result<PdfDocument>?>(null, path) {
        val opened = withContext(pdfThread) { runCatching { opener(path) } }
        value = opened
        awaitDispose { opened.getOrNull()?.let { d -> pdfScope.launch { d.close() } } }
    }
    val fade = Modifier.graphicsLayer { val t = progress(); alpha = t; scaleX = lerp(.85f, 1f, t); scaleY = lerp(.85f, 1f, t) }
    val result = doc
    if (result != null && result.isFailure) {
        FileCard(m, FileCardState(appText("无法显示这个 PDF", "Can't show this PDF"), FileCardAction(appText("用其它应用打开", "Open in another app"), onOpenWith)), fade) {}
        return
    }
    val list = rememberLazyListState()
    val scope = rememberCoroutineScope()
    var width by remember { mutableIntStateOf(0) }
    // Sharper pages once zoomed in (re-drawn when the pinch settles past the threshold).
    val sharp by remember(zoom) { derivedStateOf { if (zoom.scale > 1.6f) 2 else 1 } }
    // The pages show between the close button and save / share (the chrome never covers a page).
    val padding = PaddingValues(top = WindowInsets.statusBars.asPaddingValues().calculateTopPadding() + 64.dp,
        bottom = WindowInsets.navigationBars.asPaddingValues().calculateBottomPadding() + CHROME_BOTTOM + 8.dp)
    Box(Modifier.fillMaxSize().then(fade).padding(padding).clipToBounds()
        .onSizeChanged { width = it.width; zoom.viewport = Size(it.width.toFloat(), it.height.toFloat()); zoom.content = zoom.viewport }
        .zoomGestures(zoom) { rest -> list.dispatchRawDelta(-rest / zoom.scale) }
        .pointerInput(Unit) {
            detectTapGestures(onDoubleTap = { at -> scope.launch { zoom.toggle(at - Offset(size.width / 2f, size.height / 2f)) } })
        }
        .semantics { contentDescription = m.name }) {
        val pdf = result?.getOrNull()
        // Opening takes a moment (the page sizes are read first); the page stays dark meanwhile.
        if (pdf == null) return@Box
        LazyColumn(Modifier.fillMaxSize().graphicsLayer {
            scaleX = zoom.scale; scaleY = zoom.scale
            translationX = zoom.offset.x; translationY = zoom.offset.y
        }, state = list, verticalArrangement = Arrangement.spacedBy(8.dp), contentPadding = PaddingValues(vertical = 8.dp)) {
            items(pdf.pageSizes.size) { i ->
                val size = pdf.pageSizes[i]
                val px = width * sharp
                val key = "$path:$i:$px"
                val page by produceState<ImageBitmap?>(PdfPageCache.get(key), key) {
                    if (px <= 0 || value != null) return@produceState
                    value = withContext(pdfThread) { runCatching { pdf.render(i, px) }.getOrNull() }?.also { PdfPageCache.put(key, it) }
                }
                // The lower-resolution picture stands in while the sharper one is drawn.
                val shown = page ?: PdfPageCache.get("$path:$i:$width")
                Box(Modifier.fillMaxWidth().aspectRatio(size.width / size.height.coerceAtLeast(1f)).background(Color.White)
                    .semantics { contentDescription = appText("第 ${i + 1} 页", "Page ${i + 1}") }) {
                    shown?.let { Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.FillBounds) }
                }
            }
        }
    }
}
