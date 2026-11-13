package com.hachiwii.oink

import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import com.hachiwii.oink.ui.*
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** File cards on the transfer page and the folder page: buttons, taps, 已发送, picture width. */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
// A plain Application: OinkApplication would start the connection service and load the Rust core.
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class FileCardUiTest {
    @get:Rule val compose = createComposeRule()

    private val windowsPc = PeerUi("windowsPc", "Windows PC", "windows", latency = 4.0)
    private fun file(id: Long, progress: Int? = null, failure: String? = null, unavailable: String? = null, batch: String? = null) =
        MessageUi(id, false, "windowsPc", "Windows PC", "windows", true, "file", name = "文件$id.pdf", detail = "PDF · 1 MB", progress = progress,
            downloadFailure = failure, unavailable = unavailable, batch = batch)
    private fun own(id: Long, kind: String = "file", sent: Boolean = true, ended: String? = null, direct: Boolean = false) =
        MessageUi(id, true, "self", "Phone", "android", true, kind, name = "我的$id", detail = "PDF · 1 MB", sent = sent, ended = ended,
            progress = if (sent) null else 40, direct = direct || ended != null, toName = "Pixel".takeIf { direct || ended != null })
    private fun image(id: Long, self: Boolean) = MessageUi(id, self, if (self) "self" else "windowsPc", if (self) "Phone" else "Windows PC",
        if (self) "android" else "windows", true, "image", name = "照片$id.jpg", progress = 100, aspect = 4f / 3f)

    private fun page(messages: List<MessageUi>, actions: AppActions = AppActions(), init: UiInit = UiInit(tab = Tab.Transfer)) = compose.setContent {
        OinkApp(AppModel(peers = listOf(windowsPc), messages = messages), init, Prefs(scheme = "light"), {}, actions, false)
    }

    @Test fun downloadAndRetryFetchTheFileOtherStatesTakeNoTap() {
        val fetched = mutableListOf<Long>()
        page(listOf(file(1), file(2, failure = "下载失败"), file(3, unavailable = "offline"), file(4, progress = 100), file(5, progress = 45)),
            AppActions(download = { fetched += it.id }))
        compose.onNodeWithContentDescription("下载 文件1.pdf").performClick()
        compose.onNodeWithContentDescription("重试下载 文件2.pdf").performClick()
        assertEquals(listOf(1L, 2L), fetched)
        compose.onNodeWithText("下载失败").assertExists()
        compose.onNodeWithText("暂时无法加载").assertExists()
        // Source offline, downloaded, downloading: no download button.
        for (id in 3..5) compose.onAllNodesWithContentDescription("下载 文件$id.pdf").assertCountEquals(0)
    }

    @Test fun expiredContentIgnoresTaps() {
        val opened = mutableListOf<Long>()
        page(listOf(file(1, unavailable = "expired")), AppActions(openFile = { opened += it.id }))
        compose.onNodeWithText("已失效").assertExists()
        compose.onAllNodesWithContentDescription("下载 文件1.pdf").assertCountEquals(0)
        compose.onNodeWithText("文件1.pdf").performClick()
        compose.waitForIdle()
        // Neither the viewer nor another app.
        compose.onAllNodesWithContentDescription("关闭").assertCountEquals(0)
        assertEquals(emptyList<Long>(), opened)
    }

    @Test fun expandedBatchFilesDownloadOneByOne() {
        val fetched = mutableListOf<Long>()
        val batch = (1L..3L).map { file(it, batch = "windowsPc:02") }
        page(batch, AppActions(download = { fetched += it.id }), UiInit(tab = Tab.Transfer, transferExpanded = setOf("windowsPc:02")))
        compose.onNodeWithContentDescription("下载 文件2.pdf").performClick()
        assertEquals(listOf(2L), fetched)
    }

    @Test fun aCollapsedStackShowsNoDownloadState() {
        page((1L..3L).map { file(it, progress = if (it == 2L) 45 else null, batch = "windowsPc:02") })
        compose.onNodeWithContentDescription("展开全部 3 项").assertExists()
        compose.onAllNodesWithContentDescription("下载", substring = true).assertCountEquals(0)
        // Not even the ring of the file that is downloading.
        compose.onAllNodesWithText("45").assertCountEquals(0)
    }

    @Test fun everyCompletedPhoneSendNamesTheRecipientAndPcSendsHaveNoCaption() {
        page(listOf(own(1, direct = true), own(2, "image", direct = true), own(3, "folder", ended = "failed"), own(4)))
        compose.onAllNodesWithText("已发送给 Pixel").assertCountEquals(2)
        compose.onAllNodesWithText("已发送").assertCountEquals(0)
        compose.onNodeWithText("对方已断开，发送失败").assertExists()
    }

    @Test fun pcSendsAndAnInFlightPhoneSendHaveNoSuccessCaption() {
        page(listOf(own(1), own(2, sent = false, direct = true)))
        compose.onAllNodesWithText("已发送").assertCountEquals(0)
        compose.onNodeWithText("正在发送给 Pixel · 40%").assertExists()
    }

    @Test fun receivedAndOwnPicturesAre220Wide() {
        page(listOf(image(1, self = false), image(2, self = true)))
        val pictures = compose.onAllNodesWithContentDescription("打开图片")
        pictures.assertCountEquals(2)
        for (i in 0..1) pictures[i].getUnclippedBoundsInRoot().let { assertEquals(220f, (it.right - it.left).value, .5f) }
    }

    @Test fun folderRowsDownloadRetryEnterAndAskForThumbnails() {
        val calls = mutableListOf<String>()
        val folder = FolderUi("现场资料", listOf(
            FolderItemUi("1", "image", "现场 02.jpg", "2.9 MB", entryId = 1),
            FolderItemUi("2", "file", "验收单.pdf", "1.1 MB", entryId = 2),
            FolderItemUi("3", "file", "原始照片.zip", "486 MB", entryId = 3, failure = "下载失败"),
            FolderItemUi("4", "folder", "图纸", "", entryId = 4),
        ), messageId = 9)
        page(emptyList(), AppActions(folderDownload = { _, item, open -> calls += "download:${item.entryId}:$open" },
            folderThumbnail = { _, item -> calls += "thumbnail:${item.entryId}" }, folderEnter = { _, item -> calls += "enter:${item.entryId}" }),
            UiInit(tab = Tab.Transfer, folder = folder))
        compose.waitForIdle()
        compose.onNodeWithText("下载失败").assertExists()
        compose.onNodeWithContentDescription("下载 验收单.pdf").performClick()
        compose.onNodeWithContentDescription("重试下载 原始照片.zip").performClick()
        compose.onNodeWithText("图纸").performClick()
        // Only the picture without a thumbnail asks for one.
        assertEquals(listOf("thumbnail:1", "download:2:false", "download:3:false", "enter:4"), calls)
    }
}
