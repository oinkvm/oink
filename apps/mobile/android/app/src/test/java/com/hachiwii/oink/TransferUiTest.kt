package com.hachiwii.oink

import android.content.ComponentName
import android.content.pm.PackageManager
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.semantics.getOrNull
import androidx.compose.ui.test.assert
import androidx.compose.ui.test.SemanticsMatcher
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeLeft
import androidx.compose.ui.test.swipeRight
import com.hachiwii.oink.ui.*
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** Transfer page: batches as stacks, long text folding, the message menu, selection, the preview's save button, the composer hint. */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
// A plain Application: OinkApplication would start the connection service and load the Rust core.
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class TransferUiTest {
    @get:Rule val compose = createComposeRule()

    private val windowsPc = PeerUi("windowsPc", "Windows PC", "windows", latency = 4.0)
    private fun image(id: Long, batch: String?, self: Boolean = false) = MessageUi(id, self, if (self) "self" else "windowsPc", if (self) "Phone" else "Windows PC",
        if (self) "android" else "windows", true, "image", name = "照片$id.jpg", progress = 100, imagePath = "/test/photo$id.jpg", batch = batch)
    private fun file(id: Long, batch: String?) = MessageUi(id, false, "windowsPc", "Windows PC", "windows", true, "file", name = "文件$id.pdf", detail = "PDF · 1 MB", batch = batch)
    private fun text(id: Long, text: String) = MessageUi(id, false, "windowsPc", "Windows PC", "windows", true, "text", text = text, copyable = true)
    private val longText = (1..25).joinToString("\n") { "第 $it 行" }

    private fun page(messages: List<MessageUi>, actions: AppActions = AppActions(), init: UiInit = UiInit(tab = Tab.Transfer),
                     peers: List<PeerUi> = listOf(windowsPc)) = compose.setContent {
        OinkApp(AppModel(peers = peers, messages = messages), init, Prefs(scheme = "light"), {}, actions, false)
    }

    // ---- grouping ----

    @Test fun consecutiveMessagesOfOneBatchStack() {
        val messages = listOf(image(1, "a:01"), image(2, "a:01"), image(3, "a:01"), text(4, "hi"), file(5, "a:02"), image(6, "a:03"), file(7, null))
        val rows = transferRows(messages, emptySet())
        assertEquals(listOf("stack", "single", "single", "single", "single"), rows.map { if (it is StackRow) "stack" else "single" })
        assertEquals(listOf(1L, 2L, 3L), (rows[0] as StackRow).items.map { it.id })
        // Expanded: every message on its own row again.
        assertEquals(7, transferRows(messages, setOf("a:01")).size)
    }

    @Test fun batchKeyIsTheOperationPerSenderForContentOnly() {
        val op = ByteArray(16) { it.toByte() }
        assertEquals(transferBatch(false, "pc", "image", op), transferBatch(false, "pc", "file", op))
        assertNotEquals(transferBatch(false, "pc", "image", op), transferBatch(false, "other", "image", op))
        assertNotEquals(transferBatch(false, "pc", "image", op), transferBatch(true, "pc", "image", op))
        assertNull(transferBatch(false, "pc", "text", op))
        assertNull(transferBatch(false, "pc", "image", ByteArray(16)))
        assertNull(transferBatch(false, "pc", "image", ByteArray(0)))
    }

    // ---- stacks ----

    private fun stack(n: Int, top: Int) = "$n 项，当前第 $top 项：照片$top.jpg。左右划动切换，轻点打开"

    @Test fun swipingChangesTheTopCardAndTapOpensIt() {
        page((1L..4L).map { image(it, "windowsPc:01") })
        compose.onNodeWithContentDescription("展开全部 4 项").assertExists()
        compose.onNodeWithContentDescription(stack(4, 1)).performTouchInput { swipeLeft() }
        compose.onNodeWithContentDescription(stack(4, 2)).performTouchInput { swipeLeft() }
        compose.onNodeWithContentDescription(stack(4, 3)).performTouchInput { swipeRight() }
        compose.onNodeWithContentDescription(stack(4, 2)).performClick()
        compose.waitForIdle()
        // The viewer opened on the top card (second of the four pictures).
        compose.onNodeWithText("2 / 4").assertExists()
        compose.onNodeWithContentDescription("关闭").assertExists()
    }

    // ---- folder page ----

    @Test fun aFolderWhoseListingFailedShowsWhyAndRetries() {
        val retried = mutableListOf<Long>()
        val failed = FolderUi("Documents", emptyList(), 34, 0, error = "无法获取文件夹内容：连接中断，请重试", retryable = true)
        page(emptyList(), AppActions(folderRetry = { retried += it.messageId }), UiInit(tab = Tab.Transfer, folder = failed))
        compose.onNodeWithText("无法获取文件夹内容：连接中断，请重试").assertExists()
        compose.onNodeWithText("重试").performClick()
        assertEquals(listOf(34L), retried)
    }

    @Test fun aFolderWhoseContentIsGoneHasNothingToRetry() {
        val gone = FolderUi("Documents", emptyList(), 34, 0, error = "文件夹内容已失效", retryable = false)
        page(emptyList(), init = UiInit(tab = Tab.Transfer, folder = gone))
        compose.onNodeWithText("文件夹内容已失效").assertExists()
        compose.onAllNodesWithText("重试").assertCountEquals(0)
    }

    @Test fun tappingAFileStackOpensItsTopFile() {
        val opened = mutableListOf<Long>()
        page((1L..3L).map { file(it, "windowsPc:02") }, AppActions(openFile = { opened += it.id }))
        compose.onNodeWithContentDescription("3 项，当前第 1 项：文件1.pdf", substring = true).performTouchInput { swipeLeft() }
        compose.onNodeWithContentDescription("3 项，当前第 2 项：文件2.pdf", substring = true).performClick()
        compose.waitForIdle()
        // The media viewer on the top file (native-rewrite.md 「全屏查看器」), not the system viewer.
        compose.onNodeWithText("2 / 3").assertExists()
        compose.onNodeWithText("下载").assertExists()
        assertEquals(emptyList<Long>(), opened)
    }

    @Test fun expandShowsEveryMessageAndHideCollapsesOntoTheTappedOne() {
        page((1L..3L).map { file(it, "windowsPc:02") })
        compose.onNodeWithContentDescription("展开全部 3 项").performClick()
        compose.waitForIdle()
        compose.onAllNodesWithContentDescription("展开全部 3 项").assertCountEquals(0)
        (1..3).forEach { compose.onNodeWithText("文件$it.pdf").assertExists() }
        // One 收起, beside the first completely visible message of the batch.
        compose.onAllNodesWithContentDescription("收起为一叠").assertCountEquals(1)
        compose.onNodeWithContentDescription("收起为一叠").performClick()
        compose.waitForIdle()
        compose.onNodeWithContentDescription("3 项，当前第 1 项：文件1.pdf", substring = true).assertExists()
    }

    @Test fun hideMakesItsMessageTheTopCard() {
        // More messages than fit: the page opens at the newest, so the first ones are scrolled away.
        page((1L..14L).map { file(it, "windowsPc:02") }, init = UiInit(tab = Tab.Transfer, transferExpanded = setOf("windowsPc:02")))
        compose.waitForIdle()
        compose.onAllNodesWithContentDescription("收起为一叠").assertCountEquals(1)
        compose.onNodeWithContentDescription("收起为一叠").performClick()
        compose.waitForIdle()
        val stack = compose.onNodeWithContentDescription("14 项，当前第", substring = true).fetchSemanticsNode()
            .config[SemanticsProperties.ContentDescription].first { it.startsWith("14 项") }
        val top = Regex("第 (\\d+) 项").find(stack)!!.groupValues[1].toInt()
        assertTrue(stack, top > 1)
        assertTrue(stack, stack.contains("：文件$top.pdf。"))
    }

    @Test fun theStackIsCircular() {
        page((1L..4L).map { image(it, "windowsPc:01") })
        // Before the first card comes the last one, after the last the first.
        compose.onNodeWithContentDescription(stack(4, 1)).performTouchInput { swipeRight() }
        compose.onNodeWithContentDescription(stack(4, 4)).performTouchInput { swipeLeft() }
        compose.onNodeWithContentDescription(stack(4, 1)).assertExists()
    }

    @Test fun stackRolesWrapAroundAndThinOutForSmallBatches() {
        assertEquals(mapOf(0 to "top", 1 to "next", 3 to "prev", 2 to "next2"), stackRoles(4, 0))
        assertEquals(mapOf(1 to "top", 0 to "next"), stackRoles(2, 1))
        assertEquals(mapOf(0 to "top", 1 to "next", 5 to "prev", 2 to "next2", 3 to "farNext", 4 to "farPrev"), stackRoles(6, 0))
        // Pictures keep their aspect inside the 120 × 160 card.
        assertEquals(Size(120f, 68f), contain(16f / 9f))
        assertEquals(Size(90f, 160f), contain(9f / 16f))
        assertEquals(Size(210f, 280f), contain(3f / 4f, 220f, 280f))
    }

    @Test fun expandingKeepsTheTopCardsMessageWhereTheStackWas() {
        // Enough above and below that the list can scroll to keep it there.
        page((1L..12L).map { text(100 + it, "消息 $it") } + (1L..5L).map { file(it, "windowsPc:02") })
        compose.onNodeWithContentDescription("5 项，当前第 1 项：文件1.pdf", substring = true).performTouchInput { swipeLeft() }
        val box = compose.onNodeWithContentDescription("5 项，当前第 2 项：文件2.pdf", substring = true).getUnclippedBoundsInRoot()
        compose.onNodeWithContentDescription("展开全部 5 项").performClick()
        compose.waitForIdle()
        // The file card (its whole surface takes the taps, so the name merges into it) lands where the stack box was.
        val row = compose.onNodeWithText("文件2.pdf").getUnclippedBoundsInRoot()
        assertEquals(box.top.value, row.top.value, 1.5f)
    }

    @Test fun aPcBatchHasNoSuccessCaption() {
        page((1L..3L).map { image(it, "self:01", self = true) }, init = UiInit(tab = Tab.Transfer, transferExpanded = setOf("self:01")))
        compose.onAllNodesWithText("已发送").assertCountEquals(0)
    }

    // ---- long text ----

    @Test fun longTextFoldsAndShowMoreAddsTwentyLines() {
        val copied = mutableListOf<MessageUi>()
        page(listOf(text(1, longText)), AppActions(copyMessage = { copied += it; true }))
        compose.onNodeWithText("查看更多").assertExists()
        // 12 above, 10 lines of 23.1 (whole pixels: 23.33 at xxhdpi), then 4 under the 36 high 查看更多 row.
        assertEquals(12f + TEXT_FOLD_LINES * 23.2f + 40f, bubbleHeight(), 2.5f)
        compose.onNodeWithText("查看更多").performClick()
        compose.waitForIdle()
        // 10 + 20 ≥ 25: everything shows, no more 查看更多.
        compose.onAllNodesWithText("查看更多").assertCountEquals(0)
        assertEquals(12f + 25 * 23.2f + 12f, bubbleHeight(), 6f)
        // Copy still takes the whole text.
        compose.onNodeWithText(longText).performTouchInput { longClick() }
        compose.onNodeWithText("复制").performClick()
        assertEquals(longText, copied.single().text)
    }

    @Test fun longPressAnywhereOnALongMessageOpensTheMenu() {
        page(listOf(text(1, longText)))
        compose.onNodeWithText("查看更多").performClick()
        compose.waitForIdle()
        compose.onNodeWithText(longText).performTouchInput { longClick(bottomCenter.copy(y = bottom - 2f)) }
        compose.onNodeWithText("选择").assertExists()
        compose.onNodeWithText("复制").assertExists()
        compose.onNodeWithText("删除").assertExists()
    }

    @Test fun selectStartsTextSelection() {
        page(listOf(text(1, "可以选择的文字")))
        compose.onNodeWithText("可以选择的文字").performTouchInput { longClick() }
        compose.onNodeWithText("选择").performClick()
        compose.waitForIdle()
        compose.onNodeWithContentDescription("选择文字").assertIsFocused()
        compose.onNodeWithContentDescription("选择文字").assert(SemanticsMatcher("whole text selected") {
            it.config.getOrNull(SemanticsProperties.TextSelectionRange)?.let { r -> r.start == 0 && r.end == "可以选择的文字".length } == true
        })
    }

    @Test fun filesOfferNoSelect() {
        page(listOf(file(1, null)))
        compose.onNodeWithText("文件1.pdf").performTouchInput { longClick() }
        compose.onNodeWithText("删除").assertExists()
        compose.onAllNodesWithText("选择").assertCountEquals(0)
    }

    private fun bubbleHeight() = compose.onNodeWithText(longText).getUnclippedBoundsInRoot().let { (it.bottom - it.top).value }

    // ---- password text ----

    private fun secret(id: Long, text: String?, progress: Int? = null) = MessageUi(id, false, "windowsPc", "Windows PC", "windows", true, "text",
        text = text ?: "正在获取…", copyable = text != null, sensitive = true, textPending = text == null, progress = progress)

    @Test fun passwordTextShowsStarsUntilRevealed() {
        val copied = mutableListOf<MessageUi>()
        page(listOf(secret(1, "hunter2")), AppActions(copyMessage = { copied += it; true }))
        compose.onNodeWithText("***").assertExists()
        compose.onAllNodesWithText("hunter2").assertCountEquals(0)
        // Copying while hidden still copies the real text (and no 选择, which would show it).
        compose.onNodeWithText("***").performTouchInput { longClick() }
        compose.onAllNodesWithText("选择").assertCountEquals(0)
        compose.onNodeWithText("复制").performClick()
        assertEquals("hunter2", copied.single().text)
        compose.onNodeWithText("显示").performClick()
        compose.onNodeWithText("hunter2").assertExists()
        compose.onNodeWithContentDescription("隐藏内容").performClick()
        compose.onNodeWithText("***").assertExists()
        compose.onAllNodesWithText("hunter2").assertCountEquals(0)
    }

    @Test fun revealedTextIsAPlainBubbleWithHideBesideIt() {
        page(listOf(secret(1, "hunter2"), text(2, "abcdefg")))
        compose.onNodeWithText("显示").performClick()
        // 隐藏 is the pill beside the bubble (like 展开 / 收起): no row, and no room kept for one.
        compose.onAllNodesWithText("显示").assertCountEquals(0)
        val height = { t: String -> compose.onNodeWithText(t).getUnclippedBoundsInRoot().let { it.bottom - it.top } }
        assertEquals(height("abcdefg").value, height("hunter2").value, .5f)
        val bubble = compose.onNodeWithText("hunter2").getUnclippedBoundsInRoot()
        val pill = compose.onNodeWithContentDescription("隐藏内容").getUnclippedBoundsInRoot()
        // A received message: the pill is on its open (right) side.
        assertTrue(pill.left >= bubble.right)
    }

    @Test fun revealingTextThatIsNotHereYetDownloadsItFirst() {
        val downloads = mutableListOf<Long>()
        page(listOf(secret(1, null)), AppActions(download = { downloads += it.id }))
        compose.onNodeWithText("显示").performClick()
        assertEquals(listOf(1L), downloads)
        // Until it arrives the bubble says so (the core's pending status), with neither 显示 nor 隐藏.
        compose.onNodeWithText("正在获取…").assertExists()
        compose.onAllNodesWithText("显示").assertCountEquals(0)
        compose.onAllNodesWithContentDescription("隐藏内容").assertCountEquals(0)
    }

    // ---- preview ----

    @Test fun saveTurnsIntoACheckForTwoSecondsAndIgnoresTaps() {
        val saved = mutableListOf<Long>()
        // The picture is decoded (the button shows once there is something to save).
        cacheImage("/test/photo1.jpg", 720, androidx.compose.ui.graphics.ImageBitmap(40, 30))
        compose.mainClock.autoAdvance = false
        page(listOf(image(1, null)), AppActions(saveImage = { saved += it.id }), UiInit(tab = Tab.Transfer, viewer = image(1, null)))
        compose.mainClock.advanceTimeBy(500)
        compose.onNodeWithContentDescription("保存到相册").performClick()
        compose.mainClock.advanceTimeBy(100)
        compose.onNodeWithContentDescription("已保存到相册").performClick()
        compose.mainClock.advanceTimeBy(100)
        assertEquals(listOf(1L), saved)
        compose.mainClock.advanceTimeBy(SAVE_CONFIRM_MS)
        compose.onNodeWithContentDescription("保存到相册").performClick()
        compose.mainClock.advanceTimeBy(100)
        assertEquals(listOf(1L, 1L), saved)
    }

    @Test fun closingThePreviewAnimatesBeforeItGoes() {
        compose.mainClock.autoAdvance = false
        page(listOf(image(1, null)), init = UiInit(tab = Tab.Transfer, viewer = image(1, null)))
        compose.mainClock.advanceTimeBy(500)
        compose.onNodeWithContentDescription("关闭").performClick()
        compose.mainClock.advanceTimeBy(100)
        compose.onNodeWithContentDescription("关闭").assertExists()
        compose.mainClock.advanceTimeBy(400)
        compose.onAllNodesWithContentDescription("关闭").assertCountEquals(0)
    }

    // ---- composer: where to send (phone-to-phone, 2026-11-11) ----

    private val macMini = PeerUi("macMini", "Mac mini", "mac", offline = true)
    private val pixel = PeerUi("pixel", "Pixel", "android")
    private val ipad = PeerUi("ipad", "iPad", "ipad")

    private fun targets(vararg peers: PeerUi) = transferTargetsOf(peers.toList())

    @Test fun onlyOneOtherDeviceMeansNoPicker() {
        // Online: 「发送给 X」 (also for a computer: nothing else to paste on).
        composerTarget(targets(windowsPc), listOf(windowsPc), null).let {
            assertFalse(it.picker); assertFalse(it.disabled); assertEquals("windowsPc", it.target?.id); assertEquals("发送给 Windows PC", it.placeholder)
        }
        // Offline: 「X 不在线」, input and + off.
        composerTarget(targets(macMini), listOf(macMini), null).let {
            assertFalse(it.picker); assertTrue(it.disabled); assertEquals("Mac mini 不在线", it.placeholder)
        }
    }

    @Test fun thePickerSaysWhetherPastingAnywhereApplies() {
        val peers = listOf(windowsPc, macMini, pixel)
        composerTarget(targets(*peers.toTypedArray()), peers, "windowsPc").let {
            assertTrue(it.picker); assertEquals("发送给 Windows PC，可在所有设备粘贴", it.placeholder)
        }
        composerTarget(targets(*peers.toTypedArray()), peers, "pixel").let { assertEquals("发送给 Pixel", it.placeholder) }
    }

    @Test fun aChosenDeviceThatWentOfflineHandsOverComputersFirst() {
        val peers = listOf(pixel, macMini, windowsPc)
        // Mac mini (chosen) is offline: Windows PC, the online computer, takes over before the phone.
        composerTarget(targets(*peers.toTypedArray()), peers, "macMini").let {
            assertEquals("windowsPc", it.target?.id); assertEquals("macMini", it.switchedFrom?.id)
        }
        // Nothing online at all: the ban sign, disabled, 「没有其它设备在线」.
        val off = listOf(macMini, pixel.copy(offline = true))
        composerTarget(targets(*off.toTypedArray()), off, "macMini").let {
            assertTrue(it.picker); assertTrue(it.noneOnline); assertTrue(it.disabled); assertEquals("没有其它设备在线", it.placeholder); assertEquals("macMini",it.switchedFrom?.id)
        }
        // No other device at all: still the picker, disabled.
        composerTarget(targets(), emptyList(), null).let { assertTrue(it.picker); assertTrue(it.noneOnline) }
    }

    @Test fun theSwitchShowsOneHintAndIsRemembered() {
        val chosen = mutableListOf<String?>()
        page(emptyList(), AppActions(chooseTarget = { chosen += it }), UiInit(tab = Tab.Transfer, transferProxy = "macMini"), peers = listOf(pixel, macMini, windowsPc))
        compose.onNodeWithText("Mac mini 已离线，已切回当前活跃").assertExists()
        compose.onNodeWithText("发送给 Windows PC，可在所有设备粘贴").assertExists()
        assertEquals(listOf<String?>(null), chosen)
    }

    @Test fun thePickerListsOnlineDevicesInTwoGroups() {
        page(emptyList(), peers = listOf(windowsPc, macMini, pixel, ipad))
        compose.onNodeWithContentDescription("发送对象", substring = true).performClick()
        compose.onNodeWithText("电脑").assertExists()
        compose.onNodeWithText("移动设备").assertExists()
        // Offline devices are not listed.
        compose.onAllNodesWithText("Mac mini").assertCountEquals(0)
        compose.onNodeWithText("Pixel").performClick()
        compose.onNodeWithText("发送给 Pixel").assertExists()
    }

    @Test fun nothingOnlineDisablesTheComposer() {
        page(emptyList(), peers = listOf(macMini, pixel.copy(offline = true)))
        compose.onNodeWithText("没有其它设备在线").assertExists()
        compose.onAllNodesWithContentDescription("添加附件").assertCountEquals(0)
    }

    /** 「文件夹」 is the attachment menu's last item and picks a folder for the chosen device (user 2026-11-11). */
    @Test fun theAttachmentMenuEndsWithFolder() {
        val picked = mutableListOf<String?>()
        page(emptyList(), AppActions(pickFolder = { picked += it }))
        compose.onNodeWithContentDescription("添加附件").performClick()
        val tops = listOf("拍摄", "图库", "文件", "文件夹").map { compose.onNodeWithText(it).getUnclippedBoundsInRoot().top }
        assertTrue("top to bottom: $tops", tops.zipWithNext().all { (above, below) -> above < below })
        compose.onNodeWithText("文件夹").performClick()
        assertEquals(listOf<String?>("windowsPc"), picked)
        compose.onAllNodesWithText("文件夹").assertCountEquals(0)
    }

    // ---- phone-to-phone messages ----

    private fun sentFile(id: Long, progress: Int?, sent: Boolean, to: String? = "Pixel", ended: String? = null) =
        MessageUi(id, true, "self", "Phone", "android", true, "file", name = "现场照片.zip", detail = "ZIP · 486 MB",
            progress = progress, sent = sent, direct = to != null, toName = to, ended = ended)
    private fun directFile(id: Long, progress: Int?, ended: String? = null) =
        MessageUi(id, false, "pixel", "Pixel", "android", true, "file", name = "合同.pdf", detail = "PDF · 1 MB", progress = progress, direct = true, ended = ended)

    @Test fun ownMessagesSayWhereTheyWent() {
        // 已发送给 X shows under the newest own message once it got through; earlier ones still sending show their progress.
        page(listOf(sentFile(1, 45, false), sentFile(2, 30, false, to = null), sentFile(3, 100, true)), peers = listOf(windowsPc, pixel))
        compose.onNodeWithText("已发送给 Pixel").assertExists()
        compose.onNodeWithText("正在发送给 Pixel · 45%").assertExists()
        compose.onNodeWithText("发送进度 30%").assertExists()
    }

    @Test fun olderDirectSuccessRemainsWhileTheNewestIsStillSending() {
        // The newest own message is still on its way: no earlier one says 已发送给 X (native-rewrite.md 「发送状态行「已发送」」).
        page(listOf(sentFile(1, 100, true), sentFile(2, 45, false)), peers = listOf(windowsPc, pixel))
        compose.onAllNodesWithText("已发送给 Pixel").assertCountEquals(1)
        compose.onNodeWithText("正在发送给 Pixel · 45%").assertExists()
    }

    @Test fun earlyEndsShowOnBothSides() {
        page(listOf(sentFile(1, 62, false, ended = "failed"), directFile(2, 62, ended = "failed"), directFile(3, 62, ended = "cancelled")), peers = listOf(windowsPc, pixel))
        compose.onNodeWithText("对方已断开，发送失败").assertExists()
        compose.onNodeWithText("对方已断开，接收失败").assertExists()
        compose.onNodeWithText("已取消").assertExists()
    }

    @Test fun longPressingASendInProgressOffersOnlyCancel() {
        val cancelled = mutableListOf<Long>()
        page(listOf(sentFile(1, 45, false)), AppActions(cancelTransfer = { cancelled += it.id }), peers = listOf(windowsPc, pixel))
        compose.onNodeWithText("现场照片.zip").performTouchInput { longClick() }
        compose.onAllNodesWithText("删除").assertCountEquals(0)
        compose.onAllNodesWithText("复制").assertCountEquals(0)
        compose.onNodeWithText("取消发送").performClick()
        assertEquals(listOf(1L), cancelled)
    }

    @Test fun longPressingADirectReceiveOffersCancelThenDelete() {
        val cancelled = mutableListOf<Long>()
        page(listOf(directFile(1, 45)), AppActions(cancelTransfer = { cancelled += it.id }), peers = listOf(windowsPc, pixel))
        // It arrives by itself: no download button.
        compose.onAllNodesWithContentDescription("下载 合同.pdf").assertCountEquals(0)
        compose.onNodeWithText("合同.pdf").performTouchInput { longClick() }
        compose.onNodeWithText("删除").assertExists()
        compose.onNodeWithText("取消接收").performClick()
        assertEquals(listOf(1L), cancelled)
    }

    @Test fun aReceivedPictureKeepsItsSizeWhenItArrives() {
        val path = "/test/arrived.jpg"
        // Before: placeholder; after: the decoded picture (another aspect) in the same 260-wide box.
        var message by mutableStateOf(image(1, null).copy(imagePath = null, progress = 40))
        compose.setContent { OinkApp(AppModel(peers = listOf(windowsPc), messages = listOf(message)), UiInit(tab = Tab.Transfer), Prefs(scheme = "light"), {}, AppActions(), false) }
        val before = compose.onNodeWithContentDescription("打开图片").getUnclippedBoundsInRoot()
        cacheImage(path, 720, androidx.compose.ui.graphics.ImageBitmap(300, 400))
        message = image(1, null).copy(imagePath = path, progress = 100)
        compose.waitForIdle()
        val after = compose.onNodeWithContentDescription("打开图片").getUnclippedBoundsInRoot()
        assertEquals((before.right - before.left).value, (after.right - after.left).value, .5f)
        assertEquals((before.bottom - before.top).value, (after.bottom - after.top).value, .5f)
    }

    @Test fun noticeTextsNameTheFileOrCount() {
        assertEquals("发来 会议纪要.docx", receivedNoticeText(1, "会议纪要.docx"))
        assertEquals("发来 3 个文件：白板照片.jpg 等", receivedNoticeText(3, "白板照片.jpg"))
        assertEquals("3 张照片", itemsSummary(listOf("a.jpg", "b.jpg", "c.jpg"), 3))
        assertEquals("合同.pdf", itemsSummary(listOf("合同.pdf"), 0))
        assertEquals("2 个文件", itemsSummary(listOf("a.jpg", "b.pdf"), 1))
        assertEquals("正在发送给 iPhone", sendingNoticeTitle("iPhone"))
        assertEquals("3 张照片 · 52%", sendingNoticeText("3 张照片", 52))
    }

    // ---- share picker ----

    @Test fun pickerRowsAreCachedWhileSearchingThenOnlineOnly() {
        val windowsPcOn = windowsPc; val macMiniOld = macMini.copy(offline = false); val iphone = PeerUi("iphone", "iPhone", "iphone")
        val searching = pickRows(listOf(windowsPcOn, macMiniOld, iphone), online = listOf(pixel), searching = true, removed = setOf("macMini"))
        // Computers first; Mac mini was found not online and left; Pixel is confirmed.
        assertEquals(listOf("windowsPc", "pixel", "iphone"), searching.map { it.peer.id })
        assertEquals(listOf(false, true, false), searching.map { it.confirmed })
        assertEquals(listOf("pixel"), pickRows(listOf(windowsPcOn), online = listOf(pixel), searching = false, removed = emptySet()).map { it.peer.id })
    }

    @Test fun pickingACachedDeviceShowsASpinnerAndDisablesTheList() {
        val picked = mutableListOf<String>()
        var connecting by mutableStateOf<String?>(null)
        compose.setContent {
            OinkTheme(OinkColors.of(false, Accent.Pink)) {
                SharePicker("3 张照片", searching = true, rows = listOf(PickRow(windowsPc, false), PickRow(pixel, false)), connecting = connecting, toast = null,
                    onPick = { connecting = it.peer.id; picked += it.peer.id }, onDismiss = {})
            }
        }
        compose.onNodeWithText("正在查找设备…").assertExists()
        compose.onNodeWithContentDescription("Windows PC").performClick()
        compose.onNodeWithContentDescription("Windows PC").assert(SemanticsMatcher.expectValue(SemanticsProperties.StateDescription, "正在连接 Windows PC"))
        // While one is being confirmed the others wait.
        compose.onNodeWithContentDescription("Pixel").performClick()
        assertEquals(listOf("windowsPc"), picked)
    }

    @Test fun anEmptyPickerSaysToOpenOink() {
        compose.setContent {
            OinkTheme(OinkColors.of(false, Accent.Pink)) {
                SharePicker("3 张照片", searching = false, rows = emptyList(), connecting = null, toast = null, onPick = {}, onDismiss = {})
            }
        }
        compose.onNodeWithText("没有在线的设备").assertExists()
        compose.onNodeWithText("请在其它设备上打开 Oink").assertExists()
    }

    // ---- open with ----

    @Test fun openWithEntriesAreNamedFileTransfer() {
        val context = RuntimeEnvironment.getApplication()
        val pm = context.packageManager
        for (alias in listOf("OpenImages", "OpenVideos", "OpenAudio", "OpenDocuments", "OpenArchives", "OpenPackages", "OpenOther")) {
            val info = pm.getActivityInfo(ComponentName(context, "com.hachiwii.oink.$alias"), PackageManager.MATCH_DISABLED_COMPONENTS)
            assertEquals(alias, "文件传输", info.loadLabel(pm).toString())
        }
    }
}
