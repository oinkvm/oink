package com.hachiwii.oink

import android.graphics.Bitmap
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Shader
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.doubleClick
import com.github.takahirom.roborazzi.captureRoboImage
import com.hachiwii.oink.ViewerFixtures.item
import com.hachiwii.oink.ViewerFixtures.picture
import com.hachiwii.oink.ui.*
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The media viewer's pages for review (always dark, so one theme): picture (fit and zoomed),
 * video (playing with its controls, paused), PDF, mid-swipe between two pages, a small picture
 * downloading (thumbnail with a small ring, ring alone), and the file card: 下载, downloading,
 * failed with 重试, 用其它应用打开. PNGs under build/outputs/roborazzi/viewer.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class MediaViewerScreenshotTest {
    @get:Rule val compose = createComposeRule()
    @get:Rule val folder = TemporaryFolder()

    private val photo by lazy { item(1, "image", "IMG_2041.HEIC", "image/heic", size = 3_100_000, local = picture("/t/landscape.heic", 1600, 1200, 200f)) }
    private val portrait by lazy { item(2, "image", "IMG_2042.jpg", "image/jpeg", size = 2_400_000, local = picture("/t/portrait.jpg", 900, 1600, 20f)) }
    private val video = item(3, "file", "演示录屏.mp4", "video/mp4", size = 48_000_000, local = "/t/demo.mp4", detail = "MP4 · 48.0 MB")
    private val pdf = item(4, "file", "季度报告.pdf", "application/pdf", size = 2_400_000, local = "/t/report.pdf", detail = "PDF · 2.4 MB")

    /** The PC's 512 × 320 preview JPEG of a picture not downloaded yet (a real file: the viewer decodes it). */
    private fun thumbnailFile(): String {
        val bitmap = Bitmap.createBitmap(512, 320, Bitmap.Config.ARGB_8888)
        android.graphics.Canvas(bitmap).drawRect(0f, 0f, 512f, 320f, Paint().apply {
            shader = LinearGradient(0f, 0f, 512f, 320f, 0xFF6C8CF5.toInt(), 0xFFF2A6C2.toInt(), Shader.TileMode.CLAMP)
        })
        val file = folder.newFile("thumbnail.png")
        file.outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }
        return file.absolutePath.also { cacheImage(it, 720, bitmap.asImageBitmap()) }
    }

    private fun open(messages: List<MessageUi>, opened: MessageUi, player: FakeVideo = FakeVideo(ViewerFixtures.frame())) {
        compose.mainClock.autoAdvance = false
        compose.setContent {
            CompositionLocalProvider(LocalVideoPlayers provides { _, _ -> player }, LocalPdfOpener provides { FakePdf(4) }) {
                OinkApp(AppModel(peers = listOf(PeerUi("windowsPc", "Windows PC", "windows")), messages = messages), UiInit(tab = Tab.Transfer, viewer = opened, freeze = true),
                    Prefs(scheme = "dark"), {}, AppActions(), true)
            }
        }
        // Past the 320 ms opening.
        compose.mainClock.advanceTimeBy(600)
    }

    private fun shot(name: String) = compose.onRoot().captureRoboImage("build/outputs/roborazzi/viewer/$name.png")

    /** Lets work off the main thread (PDF opening and drawing) land. */
    private fun settle(done: () -> Boolean) {
        repeat(300) { if (done()) return; compose.mainClock.advanceTimeBy(32); Thread.sleep(5) }
    }

    @Test fun image() { open(listOf(photo, portrait, video, pdf), photo); shot("01-image") }

    @Test fun imagePortrait() { open(listOf(photo, portrait, video, pdf), portrait); shot("02-image-portrait") }

    /**
     * A stack of four with a folder in it: the second picture is 2 / 3 (only the items the viewer
     * pages through count, so the folder leaves no gap).
     */
    @Test fun stackItem() {
        val pictures = (1..3).map { i ->
            item(20L + i, "image", "IMG_31$i.jpg", "image/jpeg", size = 2_000_000, batch = "s", local = picture("/t/stack$i.jpg", 1200, 900, 60f * i + 100f))
        }
        val stack = listOf(pictures[0], item(29, "folder", "原图", batch = "s", detail = "12 个文件")) + pictures.drop(1)
        open(listOf(photo) + stack + video, pictures[1])
        shot("18-stack-item")
    }

    @Test fun imageZoomed() {
        open(listOf(photo, portrait, video, pdf), photo)
        compose.onNodeWithContentDescription(photo.name).performTouchInput { doubleClick(Offset(width * .66f, height * .36f)) }
        compose.mainClock.advanceTimeBy(600)
        shot("03-image-zoomed")
    }

    @Test fun videoPlaying() { open(listOf(photo, video), video); shot("04-video-playing") }

    @Test fun videoPlayingControlsHidden() {
        open(listOf(photo, video), video)
        compose.mainClock.advanceTimeBy(VIDEO_CONTROLS_MS + 500)
        shot("05-video-playing-no-controls")
    }

    @Test fun videoPaused() {
        open(listOf(photo, video), video, FakeVideo(ViewerFixtures.frame(), position = 47_000))
        compose.onNodeWithContentDescription("暂停").performClick()
        compose.mainClock.advanceTimeBy(300)
        shot("06-video-paused")
    }

    @Test fun pdfPages() {
        open(listOf(photo, pdf), pdf)
        settle { compose.onAllNodesWithContentDescription("第 2 页").fetchSemanticsNodes().isNotEmpty() }
        compose.mainClock.advanceTimeBy(200)
        shot("07-pdf")
    }

    @Test fun midSwipe() {
        open(listOf(photo, portrait, video, pdf), photo)
        compose.onRoot().performTouchInput {
            down(center)
            repeat(12) { moveBy(Offset(-width * .035f, 0f)); advanceEventTime(16) }
        }
        compose.mainClock.advanceTimeBy(100)
        shot("08-paging")
    }

    @Test fun smallPictureDownloadingWithThumbnail() {
        val m = item(5, "image", "IMG_3001.jpg", "image/jpeg", size = 4_200_000, progress = 42, downloading = true, thumbnail = thumbnailFile(), aspect = 512f / 320f)
        open(listOf(photo, m), m)
        shot("09-downloading-thumbnail")
    }

    @Test fun smallPictureDownloadingNoThumbnail() {
        val m = item(5, "image", "IMG_3002.jpg", "image/jpeg", size = 4_200_000, progress = 42, downloading = true)
        open(listOf(photo, m), m)
        shot("10-downloading-no-thumbnail")
    }

    @Test fun bigPictureWaitsForDownload() {
        val m = item(6, "image", "全景照片.jpg", "image/jpeg", size = 38_400_000, progress = 0, detail = "JPG · 38.4 MB")
        open(listOf(photo, m), m)
        shot("11-card-download-big-image")
    }

    @Test fun videoNotDownloaded() {
        val m = item(7, "file", "发布会录像.mp4", "video/mp4", size = 512_000_000, detail = "MP4 · 512.0 MB")
        open(listOf(photo, m), m)
        shot("12-card-download-video")
    }

    @Test fun videoDownloading() {
        val m = item(7, "file", "发布会录像.mp4", "video/mp4", size = 512_000_000, progress = 63, downloading = true, detail = "MP4 · 512.0 MB")
        open(listOf(photo, m), m)
        shot("13-card-downloading")
    }

    @Test fun downloadFailed() {
        val m = item(8, "file", "设计说明.pdf", "application/pdf", size = 2_400_000, failure = "来源设备离线，暂时无法获取", detail = "PDF · 2.4 MB")
        open(listOf(photo, m), m)
        shot("14-card-failed-retry")
    }

    @Test fun otherFileDownloaded() {
        val m = item(9, "file", "会议纪要.docx", size = 86_000, local = "/t/notes.docx", detail = "DOCX · 86 KB")
        open(listOf(photo, m), m)
        shot("15-card-open-in-another-app")
    }

    /** Opened from a folder listing: it pages through that folder level's files (here the second of three). */
    @Test fun insideAFolder() {
        val listing = FolderUi("项目素材", listOf(
            FolderItemUi("1", "folder", "参考图", ""),
            FolderItemUi("2", "file", "脚本.pdf", "PDF · 2.0 MB", entryId = 2, size = 2_000_000),
            FolderItemUi("3", "image", "封面.png", "1.2 MB", progress = 100, entryId = 3, path = picture("/t/cover.png", 1200, 1500, 300f), size = 1_200_000),
            FolderItemUi("4", "file", "合同.docx", "86 KB", progress = 100, entryId = 4, path = "/t/contract.docx", size = 86_000),
        ), messageId = 40, node = 7)
        compose.mainClock.autoAdvance = false
        compose.setContent {
            OinkApp(AppModel(peers = listOf(PeerUi("windowsPc", "Windows PC", "windows"))), UiInit(tab = Tab.Transfer, folder = listing, freeze = true),
                Prefs(scheme = "dark"), {}, AppActions(), true)
        }
        compose.mainClock.advanceTimeBy(300)
        compose.onNodeWithText("封面.png").performClick()
        compose.mainClock.advanceTimeBy(600)
        shot("17-folder-image")
    }

    @Test fun otherFileNotDownloaded() {
        val m = item(10, "file", "安装包.zip", size = 120_000_000, detail = "ZIP · 120.0 MB")
        open(listOf(photo, m), m)
        shot("16-card-download-other")
    }
}
