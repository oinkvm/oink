package com.hachiwii.oink

import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.doubleClick
import androidx.compose.ui.test.swipeLeft
import androidx.compose.ui.test.swipeRight
import com.hachiwii.oink.ViewerFixtures.item
import com.hachiwii.oink.ViewerFixtures.picture
import com.hachiwii.oink.ViewerFixtures.text
import com.hachiwii.oink.ui.*
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** The media viewer (native-rewrite.md 「全屏查看器」, Android): which items page, how each page looks, paging, video and PDF pages. */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
// A plain Application: OinkApplication would start the connection service and load the Rust core.
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class MediaViewerTest {
    @get:Rule val compose = createComposeRule()

    // ---- the sequence ----

    private val history = listOf(
        text(1, "你好"),
        item(2, "image", "IMG_1.jpg", "image/jpeg", local = "/t/1.jpg", batch = "a"),
        item(3, "file", "说明.pdf", "application/pdf", batch = "a"),
        item(4, "image", "IMG_2.heic", "image/heic", batch = "a", progress = 0),
        item(5, "folder", "素材"),
        item(6, "file", "clip.mov", "video/quicktime", local = "/t/6.mov"),
        text(7, "看看"),
        item(8, "file", "会议纪要.docx", local = "/t/8.docx"),
    )

    @Test fun everyFileOfTheHistoryPagesInMessageOrderAndBatchesComeApart() {
        // Text and folders stay out; the three items of batch "a" are three pages in their order.
        assertEquals(listOf(2L, 3L, 4L, 6L, 8L), viewerSequence(history, 4, history[3]).map { it.id })
    }

    @Test fun anItemOutsideTheHistoryPagesAlone() {
        val fromFolder = MessageUi(-1, false, "", "", "", true, "image", name = "a.png", progress = 100, imagePath = "/t/a.png")
        assertEquals(listOf(fromFolder), viewerSequence(history, -1, fromFolder))
    }

    @Test fun kindsComeFromTheTypeOrTheExtension() {
        assertEquals(ViewerKind.Image, viewerKind(item(1, "image", "x.heic", "image/heic")))
        assertEquals(ViewerKind.Image, viewerKind(item(1, "file", "photo.JPG")))
        assertEquals(ViewerKind.Image, viewerKind(item(1, "image", "clipboard")))
        assertEquals(ViewerKind.Video, viewerKind(item(1, "file", "a.bin", "video/mp4")))
        assertEquals(ViewerKind.Video, viewerKind(item(1, "file", "a.mkv", "application/octet-stream")))
        assertEquals(ViewerKind.Pdf, viewerKind(item(1, "file", "a.pdf")))
        assertNull(viewerKind(item(1, "image", "scan.tiff", "image/tiff")))
        assertNull(viewerKind(item(1, "file", "a.docx")))
        assertNull(viewerKind(item(1, "folder", "a.pdf")))
    }

    @Test fun pagesShowContentOnlyOnceItIsHere() {
        assertEquals(PageShow.Image, pageShow(item(1, "image", "a.jpg", local = "/t/a.jpg")))
        assertEquals(PageShow.Video, pageShow(item(1, "file", "a.mp4", local = "/t/a.mp4")))
        assertEquals(PageShow.Pdf, pageShow(item(1, "file", "a.pdf", local = "/t/a.pdf")))
        // A downloaded file the viewer cannot draw: the card with 用其它应用打开.
        assertEquals(PageShow.Card, pageShow(item(1, "file", "a.docx", local = "/t/a.docx")))
        // Not here yet: small pictures come by themselves, the rest waits for 下载.
        val small = item(1, "image", "a.jpg", size = 15_999_999, progress = 0)
        assertEquals(PageShow.ImageLoading, pageShow(small)); assertTrue(fetchesOnShow(small))
        val big = item(1, "image", "a.jpg", size = 16_000_000, progress = 0)
        assertEquals(PageShow.Card, pageShow(big)); assertFalse(fetchesOnShow(big))
        assertEquals(PageShow.Card, pageShow(item(1, "file", "a.mp4")))
        assertEquals(PageShow.Card, pageShow(item(1, "file", "a.pdf")))
        // Already downloading: no second request.
        assertFalse(fetchesOnShow(item(1, "image", "a.jpg", progress = 40, downloading = true)))
        // A failed or unreachable picture shows why on the card.
        assertEquals(PageShow.Card, pageShow(item(1, "image", "a.jpg", progress = 0, failure = "下载失败")))
        assertEquals(PageShow.Card, pageShow(item(1, "image", "a.jpg", progress = 0, unavailable = "expired")))
    }

    @Test fun cardsOfferWhatCanBeDone() {
        val calls = mutableListOf<String>()
        val actions = AppActions(download = { calls += "download:${it.id}" }, openWith = { calls += "open:${it.id}" })
        fun label(m: MessageUi) = cardState(m, actions.download, actions.openWith).let { it.button?.label ?: it.status ?: "ring:${it.ring}" }
        assertEquals("下载", label(item(1, "file", "a.mp4")))
        assertEquals("用其它应用打开", label(item(2, "file", "a.docx", local = "/t/a.docx")))
        assertEquals("ring:40", label(item(3, "file", "a.mp4", progress = 40, downloading = true)))
        assertEquals("重试", label(item(4, "file", "a.mp4", failure = "下载失败")))
        assertEquals("下载失败", cardState(item(4, "file", "a.mp4", failure = "下载失败"), actions.download, actions.openWith).status)
        assertEquals("内容已失效", label(item(5, "file", "a.mp4", unavailable = "expired")))
        cardState(item(6, "file", "a.mp4"), actions.download, actions.openWith).button!!.onClick()
        cardState(item(7, "file", "a.zip", local = "/t/a.zip"), actions.download, actions.openWith).button!!.onClick()
        assertEquals(listOf("download:6", "open:7"), calls)
    }

    @Test fun clockReadsMinutesAndHours() {
        assertEquals("0:00", clock(0)); assertEquals("1:23", clock(83_900)); assertEquals("1:02:03", clock(3_723_000))
    }

    // ---- paging ----

    private fun viewer(messages: () -> List<MessageUi>, opened: MessageUi, actions: AppActions = AppActions(),
                       video: (String) -> VideoController = { FakeVideo() }, pdf: (String) -> PdfDocument = { FakePdf(3) }) = compose.setContent {
        CompositionLocalProvider(LocalVideoPlayers provides { _, path -> video(path) }, LocalPdfOpener provides pdf) {
            OinkApp(AppModel(peers = listOf(PeerUi("windowsPc", "Windows PC", "windows")), messages = messages()), UiInit(tab = Tab.Transfer, viewer = opened),
                Prefs(scheme = "light"), {}, actions, false)
        }
    }

    private val photos by lazy { (1..4).map { i -> item(10L + i, "image", "P$i.jpg", "image/jpeg", local = picture("/t/p$i.jpg", 800, 600, 40f * i)) } }

    /** The page shown (only the settled page is composed). */
    private fun showing(name: String) = compose.onNodeWithContentDescription(name).assertExists()

    @Test fun opensOnTheTappedItemAndSwipesBothWays() {
        viewer({ photos }, photos[2])
        showing("P3.jpg")
        compose.onRoot().performTouchInput { swipeLeft() }
        compose.waitForIdle()
        showing("P4.jpg")
        compose.onRoot().performTouchInput { swipeRight() }
        compose.waitForIdle()
        compose.onRoot().performTouchInput { swipeRight() }
        compose.waitForIdle()
        showing("P2.jpg")
        // Single messages: no position.
        compose.onAllNodesWithText(" / ", substring = true).assertCountEquals(0)
    }

    @Test fun anItemOfAStackShowsItsPlaceInTheStack() {
        val stack = (1..3).map { i -> item(30L + i, "image", "S$i.jpg", "image/jpeg", batch = "b", local = picture("/t/s$i.jpg", 800, 600, 90f * i)) }
        val messages = listOf(photos[0]) + stack + photos[1]
        viewer({ messages }, stack[1])
        compose.onNodeWithText("2 / 3").assertExists()
        compose.onRoot().performTouchInput { swipeLeft() }
        compose.waitForIdle()
        compose.onNodeWithText("3 / 3").assertExists()
        compose.onRoot().performTouchInput { swipeLeft() }
        compose.waitForIdle()
        showing("P2.jpg")
        compose.onAllNodesWithText(" / ", substring = true).assertCountEquals(0)
        // Consecutive messages of one batch, at least two.
        assertEquals(1 to 3, batchPosition(messages, 31))
        assertNull(batchPosition(messages, photos[0].id))
        assertNull(batchPosition(listOf(item(1, "image", "a.jpg", batch = "x")), 1))
    }

    @Test fun aStackCountsOnlyTheItemsThatPage() {
        // Four items, one a folder: 1/3, 2/3, 3/3 without a gap.
        val mixed = listOf(item(1, "image", "a.jpg", batch = "m"), item(2, "folder", "素材", batch = "m"),
            item(3, "file", "b.pdf", batch = "m"), item(4, "file", "c.zip", batch = "m"))
        assertEquals(listOf(1 to 3, 2 to 3, 3 to 3), listOf(1L, 3L, 4L).map { batchPosition(mixed, it) })
        // A file and a folder: one item left to page, no position.
        val pair = listOf(item(5, "image", "d.jpg", batch = "p"), item(6, "folder", "图集", batch = "p"))
        assertNull(batchPosition(pair, 5))
        // On screen: the PDF of the mixed stack is the second of three.
        viewer({ mixed }, mixed[2])
        compose.onNodeWithText("2 / 3").assertExists()
    }

    @Test fun newMessagesJoinWithoutMovingThePageShown() {
        var messages by mutableStateOf(photos)
        viewer({ messages }, photos[1])
        showing("P2.jpg")
        val older = item(1, "image", "old.jpg", local = picture("/t/old.jpg", 800, 600, 200f))
        val newer = item(99, "file", "new.pdf", "application/pdf")
        messages = listOf(older) + photos + text(98, "hi") + newer
        compose.waitForIdle()
        // Still P2 (now the third page; the page index alone would have shown P1).
        showing("P2.jpg")
        compose.onAllNodesWithContentDescription("old.jpg").assertCountEquals(0)
    }

    @Test fun aSmallPictureDownloadsWhenItsPageShowsOthersWaitForDownload() {
        val downloads = mutableListOf<Long>()
        val items = listOf(
            item(1, "image", "small.jpg", "image/jpeg", size = 2_000_000, progress = 0),
            item(2, "image", "big.jpg", "image/jpeg", size = 30_000_000, progress = 0),
            item(3, "file", "movie.mp4", "video/mp4", size = 90_000_000),
        )
        viewer({ items }, items[0], AppActions(download = { downloads += it.id }))
        compose.waitForIdle()
        assertEquals(listOf(1L), downloads)
        compose.onRoot().performTouchInput { swipeLeft() }
        compose.waitForIdle()
        assertEquals(listOf(1L), downloads)
        compose.onNodeWithText("big.jpg").assertExists()
        compose.onNodeWithText("下载").performClick()
        compose.onRoot().performTouchInput { swipeLeft() }
        compose.waitForIdle()
        compose.onNodeWithText("下载").performClick()
        // Paging back does not ask again for the small one.
        compose.onRoot().performTouchInput { swipeRight() }
        compose.waitForIdle()
        compose.onRoot().performTouchInput { swipeRight() }
        compose.waitForIdle()
        assertEquals(listOf(1L, 2L, 3L), downloads)
    }

    @Test fun aDownloadedFileTheViewerCannotDrawOpensInAnotherApp() {
        val opened = mutableListOf<Long>()
        val shared = mutableListOf<Long>()
        val doc = item(1, "file", "会议纪要.docx", local = "/t/a.docx", detail = "DOCX · 86 KB")
        viewer({ listOf(doc) }, doc, AppActions(openWith = { opened += it.id }, shareFile = { shared += it.id }))
        // The viewer's card shows type and size in one line; the file card under the viewer shows the
        // extension and the size apart (native-rewrite.md 「文件卡片」).
        compose.onAllNodesWithText("DOCX · 86 KB").assertCountEquals(1)
        compose.onAllNodesWithText("86 KB").assertCountEquals(1)
        compose.onNodeWithText("用其它应用打开").performClick()
        // The bottom bar's 用其它应用打开 (left of 分享) does the same.
        compose.onNodeWithContentDescription("用其它应用打开").performClick()
        compose.onNodeWithContentDescription("分享").performClick()
        // No 保存到相册 for a document.
        compose.onAllNodesWithContentDescription("保存到相册").assertCountEquals(0)
        assertEquals(listOf(1L, 1L), opened); assertEquals(listOf(1L), shared)
    }

    @Test fun nothingToOpenOrShareBeforeTheDownload() {
        val clip = item(1, "file", "clip.mp4", "video/mp4", size = 90_000_000)
        viewer({ listOf(clip) }, clip)
        compose.onNodeWithText("下载").assertExists()
        compose.onAllNodesWithContentDescription("用其它应用打开").assertCountEquals(0)
        compose.onAllNodesWithContentDescription("分享").assertCountEquals(0)
        compose.onAllNodesWithContentDescription("保存到相册").assertCountEquals(0)
    }

    @Test fun aVideoPlaysOnItsPageAndPausesWhenPagedAway() {
        val player = FakeVideo()
        val items = listOf(item(1, "file", "clip.mp4", "video/mp4", local = "/t/clip.mp4"), photos[0])
        val saved = mutableListOf<Long>()
        // The clock is held: the playing video's controls hide after VIDEO_CONTROLS_MS.
        compose.mainClock.autoAdvance = false
        viewer({ items }, items[0], AppActions(saveImage = { saved += it.id }), video = { player })
        compose.mainClock.advanceTimeBy(500)
        assertEquals("play", player.calls.first())
        assertTrue(player.state.playing)
        // Controls: pause, mute; 保存到相册 for videos too.
        compose.onNodeWithContentDescription("暂停").performClick()
        compose.mainClock.advanceTimeBy(100)
        assertFalse(player.state.playing)
        compose.onNodeWithContentDescription("静音").performClick()
        compose.mainClock.advanceTimeBy(100)
        assertTrue(player.state.muted)
        compose.onNodeWithContentDescription("保存到相册").performClick()
        assertEquals(listOf(1L), saved)
        compose.onNodeWithContentDescription("播放").performClick()
        compose.mainClock.advanceTimeBy(100)
        assertTrue(player.state.playing)
        // Playing on its own, the controls go away after a while; a tap brings them back.
        compose.mainClock.advanceTimeBy(VIDEO_CONTROLS_MS + 500)
        compose.onAllNodesWithContentDescription("暂停").assertCountEquals(0)
        compose.onNodeWithContentDescription("clip.mp4").performClick()
        compose.mainClock.advanceTimeBy(100)
        compose.onNodeWithContentDescription("暂停").assertExists()
        compose.onRoot().performTouchInput { swipeLeft() }
        compose.mainClock.advanceTimeBy(2000)
        assertFalse(player.state.playing)
        assertEquals("pause", player.calls.last())
        assertTrue(player.released)
    }

    @Test fun aVideoThatCannotPlayOffersAnotherApp() {
        val player = FakeVideo().apply { state.failed = true }
        val clip = item(1, "file", "clip.mkv", local = "/t/clip.mkv")
        viewer({ listOf(clip) }, clip, video = { player })
        compose.onNodeWithText("无法播放这个视频").assertExists()
        compose.onNodeWithText("用其它应用打开").assertExists()
    }

    @Test fun aPdfShowsItsPages() {
        val pdf = FakePdf(3)
        val doc = item(1, "file", "说明.pdf", "application/pdf", local = "/t/doc.pdf")
        viewer({ listOf(doc, photos[0]) }, doc, pdf = { pdf })
        compose.waitUntil(5_000) { compose.onAllNodesWithContentDescription("第 1 页").fetchSemanticsNodes().isNotEmpty() }
        compose.onNodeWithContentDescription("第 2 页").assertExists()
        // The pages scroll vertically; a horizontal swipe still pages on.
        compose.onRoot().performTouchInput { swipeLeft() }
        compose.waitForIdle()
        showing("P1.jpg")
        compose.waitUntil(5_000) { pdf.closed }
    }

    @Test fun aPdfThatCannotOpenOffersAnotherApp() {
        val doc = item(1, "file", "locked.pdf", "application/pdf", local = "/t/locked.pdf")
        viewer({ listOf(doc) }, doc, pdf = { error("password") })
        compose.waitUntil(5_000) { compose.onAllNodesWithText("无法显示这个 PDF").fetchSemanticsNodes().isNotEmpty() }
        compose.onNodeWithText("用其它应用打开").assertExists()
    }

    @Test fun doubleTapZoomsAndStopsPagingUntilBackToFit() {
        viewer({ photos }, photos[0])
        compose.onNodeWithContentDescription("P1.jpg").performTouchInput { doubleClick() }
        compose.waitForIdle()
        // Zoomed: a swipe pans the picture instead of paging.
        compose.onRoot().performTouchInput { swipeLeft() }
        compose.waitForIdle()
        showing("P1.jpg")
        compose.onNodeWithContentDescription("P1.jpg").performTouchInput { doubleClick() }
        compose.waitForIdle()
        compose.onRoot().performTouchInput { swipeLeft() }
        compose.waitForIdle()
        showing("P2.jpg")
    }

    @Test fun zoomKeepsTheContentCoveringThePage() {
        val z = ZoomState().apply { viewport = androidx.compose.ui.geometry.Size(1000f, 2000f); content = androidx.compose.ui.geometry.Size(1000f, 750f) }
        z.transform(Offset.Zero, 2f, Offset(5000f, 5000f))
        assertEquals(2f, z.scale)
        // 2000 × 1500 in 1000 × 2000: ±500 across, nothing to pan down.
        assertEquals(Offset(500f, 0f), z.offset)
        assertEquals(1f, z.transform(Offset.Zero, 1f, Offset(0f, 1f)))
        z.transform(Offset.Zero, 10f, Offset.Zero)
        assertEquals(ZoomState.MAX_ZOOM, z.scale)
    }

    // ---- folder listings ----

    private val listing by lazy {
        FolderUi("素材", listOf(
            FolderItemUi("1", "folder", "子文件夹", ""),
            FolderItemUi("2", "image", "封面.png", "1.2 MB", progress = 100, entryId = 2, path = picture("/t/cover.png", 800, 600, 120f), size = 1_200_000),
            FolderItemUi("3", "file", "脚本.pdf", "PDF · 2.0 MB", entryId = 3, size = 2_000_000),
            FolderItemUi("4", "image", "海报.jpg", "3.0 MB", entryId = 4, size = 3_000_000),
            FolderItemUi("5", "file", "合同.docx", "86 KB", progress = 100, entryId = 5, path = "/t/contract.docx", size = 86_000),
        ), messageId = 40, node = 7)
    }

    @Test fun aFolderPagesThroughItsOwnFilesOnly() {
        val items = folderViewerItems(listing)
        // Same level, listing order, no subfolder; ids never collide with history messages.
        assertEquals(listOf("封面.png", "脚本.pdf", "海报.jpg", "合同.docx"), items.map { it.name })
        assertTrue(items.all { it.id < 0 })
        assertEquals(listOf(PageShow.Image, PageShow.Card, PageShow.ImageLoading, PageShow.Card), items.map(::pageShow))
        // A small picture of the folder fetches itself; one not started is not the placeholder artwork.
        assertTrue(fetchesOnShow(items[2]))
    }

    @Test fun openingAFileInAFolderListingPagesThatFolder() {
        val calls = mutableListOf<String>()
        compose.setContent {
            OinkApp(AppModel(peers = listOf(PeerUi("windowsPc", "Windows PC", "windows")), messages = photos), UiInit(tab = Tab.Transfer, folder = listing),
                Prefs(scheme = "light"), {}, AppActions(folderDownload = { f, item, open -> calls += "${f.messageId}:${item.entryId}:$open" },
                    download = { calls += "message:${it.id}" }), false)
        }
        compose.onNodeWithText("脚本.pdf").performClick()
        compose.waitForIdle()
        compose.onNodeWithText("下载").performClick()
        // The small picture next to it downloads by itself when its page shows.
        compose.onRoot().performTouchInput { swipeLeft() }
        compose.waitForIdle()
        showing("海报.jpg")
        // No position inside a folder.
        compose.onAllNodesWithText(" / ", substring = true).assertCountEquals(0)
        assertEquals(listOf("40:3:false", "40:4:false"), calls)
    }

    // ---- opening from the transfer page ----

    @Test fun tappingAFileOpensTheViewerAndAFolderStillBrowses() {
        val calls = mutableListOf<String>()
        val doc = item(1, "file", "报告.pdf", "application/pdf", detail = "PDF · 2.4 MB")
        val folder = item(2, "folder", "素材", detail = "12 个文件")
        compose.setContent {
            OinkApp(AppModel(peers = listOf(PeerUi("windowsPc", "Windows PC", "windows")), messages = listOf(doc, folder)), UiInit(tab = Tab.Transfer),
                Prefs(scheme = "light"), {}, AppActions(openFile = { calls += "system:${it.id}" }, openFolder = { calls += "folder:${it.id}" }), false)
        }
        compose.onNodeWithText("素材").performClick()
        assertEquals(listOf("folder:2"), calls)
        compose.onNodeWithContentDescription("返回").performClick()
        compose.onNodeWithText("报告.pdf").performClick()
        compose.waitForIdle()
        // The viewer's card for a PDF not downloaded yet, not the system viewer.
        compose.onNodeWithText("下载").assertExists()
        assertEquals(listOf("folder:2"), calls)
    }
}
