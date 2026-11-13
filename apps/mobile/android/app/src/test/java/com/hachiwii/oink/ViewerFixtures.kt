package com.hachiwii.oink

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Shader
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import com.hachiwii.oink.ui.*

/** Media viewer test data: messages, pictures in the image cache, a fake player and a fake PDF. */
internal object ViewerFixtures {
    fun item(id: Long, kind: String, name: String, mime: String? = null, size: Long = 1_000_000, local: String? = null, progress: Int? = null,
             downloading: Boolean = false, batch: String? = null, self: Boolean = false, failure: String? = null, unavailable: String? = null,
             thumbnail: String? = null, aspect: Float? = null, detail: String = "") =
        MessageUi(id, self, if (self) "self" else "windowsPc", if (self) "Phone" else "Windows PC", if (self) "android" else "windows", true, kind,
            name = name, detail = detail, progress = progress, imagePath = local?.takeIf { kind == "image" }, localPath = local, mime = mime, size = size,
            downloading = downloading, batch = batch, downloadFailure = failure, unavailable = unavailable, thumbnailPath = thumbnail, aspect = aspect,
            preview = if (kind == "image" && local == null) (if (thumbnail != null) "thumbnail" else "placeholder") else null)

    fun text(id: Long, body: String) = MessageUi(id, false, "windowsPc", "Windows PC", "windows", true, "text", text = body)

    /** A gradient "photo" with a sun, put in the image cache at both sizes the viewer uses (no decoding in tests). */
    fun picture(path: String, w: Int, h: Int, hue: Float): String {
        val bitmap = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888)
        val canvas = Canvas(bitmap)
        val top = android.graphics.Color.HSVToColor(floatArrayOf(hue, .35f, .95f)); val bottom = android.graphics.Color.HSVToColor(floatArrayOf((hue + 40f) % 360f, .55f, .7f))
        canvas.drawPaint(Paint().apply { shader = LinearGradient(0f, 0f, 0f, h.toFloat(), top, bottom, Shader.TileMode.CLAMP) })
        canvas.drawCircle(w * .68f, h * .34f, minOf(w, h) * .16f, Paint(Paint.ANTI_ALIAS_FLAG).apply { color = 0xFFFFF4D6.toInt() })
        canvas.drawRect(0f, h * .72f, w.toFloat(), h.toFloat(), Paint().apply { color = android.graphics.Color.HSVToColor(floatArrayOf((hue + 180f) % 360f, .3f, .45f)) })
        val image = bitmap.asImageBitmap()
        cacheImage(path, 720, image)
        cacheImage(path, 2048, image)
        return path
    }

    fun frame(w: Int = 640, h: Int = 360): ImageBitmap {
        val bitmap = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888)
        val canvas = Canvas(bitmap)
        canvas.drawPaint(Paint().apply { shader = LinearGradient(0f, 0f, w.toFloat(), h.toFloat(), 0xFF1F3B5C.toInt(), 0xFFE08A5B.toInt(), Shader.TileMode.CLAMP) })
        canvas.drawRect(0f, h * .78f, w.toFloat(), h.toFloat(), Paint().apply { color = 0xFF2A2D35.toInt() })
        canvas.drawCircle(w * .3f, h * .45f, h * .12f, Paint(Paint.ANTI_ALIAS_FLAG).apply { color = 0xFFFFE3B0.toInt() })
        return bitmap.asImageBitmap()
    }
}

/** The core's download status of a received test record, from its download state ("complete", "downloading", "failed"; else not downloaded). */
internal fun receivedStatus(download: String): uniffi.oink_mobile_ffi.MobileDownloadStatus = when (download) {
    "complete" -> uniffi.oink_mobile_ffi.MobileDownloadStatus.DOWNLOADED
    "downloading" -> uniffi.oink_mobile_ffi.MobileDownloadStatus.DOWNLOADING
    "failed" -> uniffi.oink_mobile_ffi.MobileDownloadStatus.DOWNLOAD_FAILED
    else -> uniffi.oink_mobile_ffi.MobileDownloadStatus.NOT_DOWNLOADED
}

/** A player that records what the page asks of it. */
internal class FakeVideo(private val picture: ImageBitmap? = null, duration: Long = 83_000, position: Long = 21_000) : VideoController {
    override val state = VideoState().apply { durationMs = duration; positionMs = position; aspect = 16f / 9f }
    val calls = mutableListOf<String>()
    var released = false
    override fun play() { calls += "play"; state.playing = true }
    override fun pause() { calls += "pause"; state.playing = false }
    override fun seekTo(ms: Long) { calls += "seek:$ms"; state.positionMs = ms }
    override fun setMuted(muted: Boolean) { calls += "mute:$muted"; state.muted = muted }
    override fun tick() {}
    override fun release() { released = true }
    @Composable override fun Picture(modifier: Modifier) {
        if (picture != null) Image(picture, null, modifier, contentScale = ContentScale.FillBounds) else Box(modifier.background(Color.DarkGray))
    }
}

/** A PDF of A4 pages with a title bar and grey "text" lines. */
internal class FakePdf(pages: Int) : PdfDocument {
    override val pageSizes = List(pages) { Size(595f, 842f) }
    var closed = false
    override fun render(index: Int, widthPx: Int): ImageBitmap {
        val h = (widthPx * 842f / 595f).toInt()
        val bitmap = Bitmap.createBitmap(widthPx, h, Bitmap.Config.ARGB_8888)
        val canvas = Canvas(bitmap)
        canvas.drawColor(android.graphics.Color.WHITE)
        val u = widthPx / 595f
        val ink = Paint().apply { color = 0xFF3A3F4B.toInt() }
        val grey = Paint().apply { color = 0xFFC9CDD6.toInt() }
        canvas.drawRect(60 * u, 70 * u, 380 * u, 92 * u, ink)
        var y = 130f
        var line = 0
        while (y < 780f) {
            val end = if (line % 6 == 5) 340f else 535f
            canvas.drawRect(60 * u, y * u, end * u, (y + 9) * u, grey)
            y += if (line % 6 == 5) 34f else 20f
            line++
        }
        canvas.drawRect(270 * u, 800 * u, 325 * u, 808 * u, Paint().apply { color = 0xFF9AA0AC.toInt() })
        return bitmap.asImageBitmap()
    }
    override fun close() { closed = true }
}
