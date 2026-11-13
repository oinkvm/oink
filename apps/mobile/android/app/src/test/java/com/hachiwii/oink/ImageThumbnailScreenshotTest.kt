package com.hachiwii.oink

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Shader
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import com.github.takahirom.roborazzi.captureRoboImage
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.junit.runner.RunWith
import org.robolectric.ParameterizedRobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import uniffi.oink_mobile_ffi.MobileImagePreview

/**
 * Image messages in every preview state (core ImagePreview): the PC's thumbnail, the thumbnail
 * while the original downloads, waiting for the thumbnail, 「图片已失效」, 「暂时无法加载」 and a
 * failed load, plus the viewer standing in with the thumbnail. Light and dark; PNGs only.
 */
@RunWith(ParameterizedRobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class ImageThumbnailScreenshotTest(private val dark: Boolean) {
    companion object {
        @JvmStatic @ParameterizedRobolectricTestRunner.Parameters(name = "dark={0}")
        fun themes() = listOf(arrayOf<Any>(false), arrayOf<Any>(true))
    }

    @get:Rule val compose = createComposeRule()
    @get:Rule val folder = TemporaryFolder()

    private val devices = listOf(
        Device("self", "Phone", true, isSelf = true, platform = "android"),
        Device("pc", "Studio-PC", true, computer = true, platform = "windows"),
        Device("mac", "MacBook", false, computer = true, platform = "macos", lastSeenMillis = System.currentTimeMillis() - 5 * 60_000),
    )
    private val now = System.currentTimeMillis()

    /** A 512 × 320 gradient, standing in for a JPEG thumbnail sent by the PC. */
    private fun thumbnailFile(): String {
        val bitmap = Bitmap.createBitmap(512, 320, Bitmap.Config.ARGB_8888)
        Canvas(bitmap).drawRect(0f, 0f, 512f, 320f, Paint().apply {
            shader = LinearGradient(0f, 0f, 512f, 320f, 0xFF6C8CF5.toInt(), 0xFFF2A6C2.toInt(), Shader.TileMode.CLAMP)
        })
        val file = folder.newFile("thumbnail.png")
        file.outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }
        return file.absolutePath
    }

    private fun image(id: Long, sender: String, preview: MobileImagePreview, thumbnail: String? = null, download: String = "placeholder",
                      availability: String = "Ready") =
        TransferMessage(id, now - (10 - id) * 60_000, sender, null, "v2:c$id", "image", "image.png", "image/png", 8_400_000, null,
            download, null, availability == "Ready", ByteArray(0), sourceAvailability = availability, imagePreview = preview,
            thumbnailPath = thumbnail, thumbnailWidth = thumbnail?.let { 512 }, thumbnailHeight = thumbnail?.let { 320 },
            // The core reports a received picture's size from its thumbnail.
            imageWidth = thumbnail?.let { 512 }, imageHeight = thumbnail?.let { 320 })

    private fun model(records: List<TransferMessage>, progress: Map<Long, TransferProgress> = emptyMap()): AppModel {
        val state = AppState(name = "Phone", transferMessages = records, transferProgress = progress)
        return AppModel(selfName = "Phone", peers = devices.filter { !it.isSelf }.map { it.toPeer(null) },
            messages = records.map { it.toUi(state, devices.associateBy { d -> d.id }) })
    }

    private fun shot(name: String, model: AppModel, init: UiInit) {
        val prefs = Prefs(scheme = if (dark) "dark" else "light")
        compose.setContent { OinkApp(model, init.copy(freeze = true), prefs, {}, AppActions(), dark) }
        compose.waitForIdle()
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/${if (dark) "dark" else "light"}/$name.png")
    }

    @Test fun thumbnailStates() {
        val thumbnail = thumbnailFile()
        val records = listOf(
            image(1, "pc", MobileImagePreview.THUMBNAIL, thumbnail),
            image(2, "pc", MobileImagePreview.THUMBNAIL, thumbnail, download = "downloading"),
            image(3, "pc", MobileImagePreview.LOADING),
            image(4, "pc", MobileImagePreview.EXPIRED, availability = "NoSuchItem"),
        )
        val model = model(records, mapOf(2L to TransferProgress(45, 100)))
        assertEquals(listOf("thumbnail", "thumbnail", "loading", "expired"), model.messages.map { it.preview })
        assertEquals(512f / 320f, model.messages[0].aspect)
        shot("transfer-image-thumbnails", model, UiInit(tab = Tab.Transfer))
    }

    @Test fun unavailableStates() {
        val records = listOf(
            image(1, "mac", MobileImagePreview.OFFLINE, availability = "SourceOffline"),
            image(2, "pc", MobileImagePreview.FAILED),
            image(3, "pc", MobileImagePreview.EXPIRED, availability = "NoSuchItem"),
        )
        val model = model(records)
        assertEquals(listOf(false, true, false), model.messages.map { it.opensViewer })
        shot("transfer-image-unavailable", model, UiInit(tab = Tab.Transfer))
    }

    @Test fun viewerShowsTheThumbnailWhileTheOriginalDownloads() {
        val records = listOf(image(1, "pc", MobileImagePreview.THUMBNAIL, thumbnailFile(), download = "downloading"))
        val model = model(records, mapOf(1L to TransferProgress(45, 100)))
        shot("transfer-image-viewer-thumbnail", model, UiInit(tab = Tab.Transfer, viewer = model.messages[0]))
    }
}
