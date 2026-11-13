package com.hachiwii.oink

import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.net.Uri
import android.os.Parcel
import android.webkit.MimeTypeMap
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import com.hachiwii.oink.privilege.ClipMarker
import com.hachiwii.oink.ui.*
import java.io.File
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** Transfer page long-press 复制: which messages offer it, what the clip holds, and the menu itself. */
@RunWith(RobolectricTestRunner::class)
// Real text layout: with the legacy graphics shadows the file row's text measures near zero wide
// and touch gestures on it miss.
@GraphicsMode(GraphicsMode.Mode.NATIVE)
// A plain Application: OinkApplication would start the connection service and load the Rust core.
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class TransferCopyTest {
    @get:Rule val compose = createComposeRule()
    private val context = RuntimeEnvironment.getApplication()
    // Shape of the core's bytes (OKC1 + identity); the hosts carry them unchanged.
    private val marker = "OKC1".toByteArray() + ByteArray(48) { (it * 5 + 3).toByte() }

    /** The core's record shapes: kind and download state as `{:?}` of the Rust enums. */
    private fun record(kind: String, text: String? = null, download: String = "Complete", path: String? = null, mime: String? = null,
                       marker: ByteArray? = this.marker) =
        TransferMessage(1, 0, "pc", null, "v2:c", kind, "item", mime, 4, text, download, path, true, ByteArray(16), clipMarker = marker)

    /** The URI shape FileProvider gives a downloaded transfer (see [imageUriComesFromTheReceivedFileProvider]). */
    private val providerUri = { file: File -> Uri.parse("content://${context.packageName}.received/transfers/c/${file.name}") }

    // Robolectric's MimeTypeMap is empty; a device knows these extensions.
    @Before fun registerImageTypes() {
        shadowOf(MimeTypeMap.getSingleton()).apply {
            addExtensionMimeTypeMapping("png", "image/png")
            addExtensionMimeTypeMapping("jpg", "image/jpeg")
        }
    }

    private fun downloadedImage(name: String = "photo.png"): File =
        File(context.filesDir, "core/transfers/c/$name").apply { parentFile!!.mkdirs(); writeBytes(byteArrayOf(1, 2, 3)) }

    private fun parcelled(clip: ClipData): ClipData {
        val parcel = Parcel.obtain()
        try {
            clip.writeToParcel(parcel, 0)
            parcel.setDataPosition(0)
            return ClipData.CREATOR.createFromParcel(parcel)
        } finally { parcel.recycle() }
    }

    private fun mimeTypes(clip: ClipData) = (0 until clip.description.mimeTypeCount).map { clip.description.getMimeType(it) }

    @Test fun onlyTextAndDownloadedImagesCanBeCopied() {
        val image = downloadedImage().path
        assertTrue(ClipboardSink.canCopy(record("Text", text = "hi")))
        assertTrue(ClipboardSink.canCopy(record("Image", path = image, mime = "image/png")))
        // An image still downloading, or a placeholder without its data.
        assertFalse(ClipboardSink.canCopy(record("Image", download = "Downloading", path = image)))
        assertFalse(ClipboardSink.canCopy(record("Image", download = "Placeholder")))
        assertFalse(ClipboardSink.canCopy(record("Image", path = File(context.filesDir, "core/transfers/c/gone.png").path)))
        assertFalse(ClipboardSink.canCopy(record("File", path = image)))
        assertFalse(ClipboardSink.canCopy(record("Folder")))
        assertFalse(ClipboardSink.canCopy(record("RichText", text = "<b>x</b>")))
        assertNull(ClipboardSink.clip(context, record("File", path = image)))
    }

    @Test fun textCarriesTheMarkerAndPastesOnlyTheText() {
        val stored = parcelled(ClipboardSink.clip(context, record("Text", text = "from pc\n"))!!)
        assertEquals(1, stored.itemCount)
        // What another app pastes: the item's text, nothing else.
        assertEquals("from pc\n", stored.getItemAt(0).coerceToText(context))
        assertEquals(listOf(ClipDescription.MIMETYPE_TEXT_PLAIN), mimeTypes(stored))
        assertArrayEquals(marker, ClipMarker.read(stored.description))
        assertFalse(stored.description.extras.containsKey(ClipDescription.EXTRA_IS_SENSITIVE))
    }

    /** User 2026-11-12: 复制 writes a text message's rich text with its plain text as one HTML clip; 复制文本
     * the plain text only. Both keep the clip marker. */
    @Test fun richCopyWritesAnHtmlClipAndPlainCopyOnlyTheText() {
        val message = record("Text", text = "bold move").copy(richTextFormat = "Html")
        val rich = uniffi.oink_mobile_ffi.MobileRichText("<b>bold</b> move", "<p><b>bold</b> move</p>", null)
        assertTrue(ClipboardSink.copyRich(context, message, rich))
        val manager = context.getSystemService(ClipboardManager::class.java)
        val written = parcelled(manager.primaryClip!!)
        assertEquals(1, written.itemCount)
        assertEquals("bold move", written.getItemAt(0).text.toString())
        assertEquals("<p><b>bold</b> move</p>", written.getItemAt(0).htmlText)
        assertTrue(written.description.hasMimeType(ClipDescription.MIMETYPE_TEXT_HTML))
        assertTrue(written.description.hasMimeType(ClipDescription.MIMETYPE_TEXT_PLAIN))
        assertArrayEquals(marker, ClipMarker.read(written.description))

        assertTrue(ClipboardSink.copyPlain(context, message))
        val plain = parcelled(manager.primaryClip!!)
        assertNull(plain.getItemAt(0).htmlText)
        assertEquals(listOf(ClipDescription.MIMETYPE_TEXT_PLAIN), mimeTypes(plain))
        assertArrayEquals(marker, ClipMarker.read(plain.description))
        // A text without rich text on this phone: 复制 writes its plain text.
        assertTrue(ClipboardSink.copyRich(context, message, null))
        assertNull(parcelled(manager.primaryClip!!).getItemAt(0).htmlText)
    }

    /** Received text with rich text (automatic write, one-tap copy) is one HTML clip with the marker. */
    @Test fun receivedRichTextIsOneHtmlClip() {
        val clip = parcelled(ClipboardSink.clip(context, ReceivedContent(text = "hi", marker = marker, html = "<i>hi</i>")))
        assertEquals(1, clip.itemCount)
        assertEquals("hi", clip.getItemAt(0).text.toString())
        assertEquals("<i>hi</i>", clip.getItemAt(0).htmlText)
        // Both types declared: apps that only look for text/plain still offer paste.
        assertEquals(listOf(ClipDescription.MIMETYPE_TEXT_PLAIN, ClipDescription.MIMETYPE_TEXT_HTML), mimeTypes(clip))
        assertArrayEquals(marker, ClipMarker.read(clip.description))
    }

    @Test fun passwordTextIsCopiedWholeAndMarkedSensitive() {
        val stored = parcelled(ClipboardSink.clip(context, record("Text", text = "hunter2").copy(sensitive = true))!!)
        assertEquals("hunter2", stored.getItemAt(0).coerceToText(context))
        assertTrue(stored.description.extras.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE))
        assertArrayEquals(marker, ClipMarker.read(stored.description))
    }

    @Test fun imageIsAContentUriUnderItsImageTypeWithTheMarker() {
        val stored = parcelled(ClipboardSink.clip(context, record("Image", path = downloadedImage().path, mime = "image/png"), providerUri)!!)
        assertEquals(1, stored.itemCount)
        assertEquals(providerUri(downloadedImage()), stored.getItemAt(0).uri)
        assertNull(stored.getItemAt(0).text)
        assertEquals(listOf("image/png"), mimeTypes(stored))
        assertArrayEquals(marker, ClipMarker.read(stored.description))
    }

    /** The real FileProvider URI (the transfers root of file_paths.xml). */
    @Test fun imageUriComesFromTheReceivedFileProvider() {
        // FileProvider matches paths by '/', so it cannot resolve a Windows host's temp paths.
        assumeTrue(File.separatorChar == '/')
        val uri = ClipboardSink.clip(context, record("Image", path = downloadedImage().path, mime = "image/png"))!!.getItemAt(0).uri
        assertEquals("content", uri.scheme)
        assertEquals("${context.packageName}.received", uri.authority)
        assertEquals("/transfers/c/photo.png", uri.path)
    }

    @Test fun imageTypeComesFromTheExtensionThenTheRecord() {
        assertEquals("image/jpeg", ClipboardSink.imageMime("a.JPG", null))
        assertEquals("image/png", ClipboardSink.imageMime("a.png", "image/jpeg"))
        assertEquals("image/heic", ClipboardSink.imageMime("a.bin", "image/heic"))
        assertNull(ClipboardSink.imageMime("a.bin", "application/octet-stream"))
    }

    @Test fun directSharesCarryNoMarker() {
        val text = ClipboardSink.clip(context, record("Text", text = "shared", marker = null))!!
        assertNull(text.description.extras)
        assertEquals("shared", text.getItemAt(0).coerceToText(context))
        val image = ClipboardSink.clip(context, record("Image", path = downloadedImage().path, mime = "image/png", marker = null), providerUri)!!
        assertNull(ClipMarker.read(image.description))
    }

    @Test fun copyWritesTheSystemClipboard() {
        assertTrue(ClipboardSink.copy(context, record("Text", text = "on the clipboard")))
        val clip = context.getSystemService(ClipboardManager::class.java).primaryClip!!
        assertEquals("on the clipboard", clip.getItemAt(0).coerceToText(context))
        assertArrayEquals(marker, ClipMarker.read(clip.description))
        assertFalse(ClipboardSink.copy(context, record("Folder")))
    }

    // The menu on the page.

    private val copied = mutableListOf<Long>()
    private val deleted = mutableListOf<Long>()
    private val messages = listOf(
        MessageUi(1, false, "pc", "Windows PC", "windows", true, "text", text = "可以复制的文字", copyable = true),
        MessageUi(2, false, "pc", "Windows PC", "windows", true, "file", name = "说明.pdf", detail = "PDF · 2 MB"),
        MessageUi(3, false, "pc", "Windows PC", "windows", true, "image", name = "图.png", progress = 40),
    )

    private fun page() = compose.setContent {
        OinkApp(AppModel(messages = messages), UiInit(tab = Tab.Transfer), Prefs(scheme = "light"), {},
            AppActions(copyMessage = { copied += it.id; true }, deleteMessage = { deleted += it.id }), false)
    }

    @Test fun textMenuOffersCopyThenDelete() {
        page()
        compose.onNodeWithText("可以复制的文字").performTouchInput { longClick() }
        compose.onNodeWithText("复制").assertExists()
        compose.onNodeWithText("删除").assertExists()
        compose.onNodeWithText("复制").performClick()
        compose.waitForIdle()
        assertEquals(listOf(1L), copied)
        compose.onAllNodesWithText("复制").assertCountEquals(0)
        // Android 13+ shows its own clipboard confirmation: no toast of ours.
        compose.onAllNodesWithText("已复制").assertCountEquals(0)
    }

    @Test @Config(sdk = [32])
    fun beforeAndroid13CopyShowsTheToast() {
        page()
        compose.onNodeWithText("可以复制的文字").performTouchInput { longClick() }
        compose.onNodeWithText("复制").performClick()
        compose.onNodeWithText("已复制").assertExists()
    }

    @Test fun filesAndUndownloadedImagesOfferOnlyDelete() {
        page()
        compose.onNodeWithText("说明.pdf").performTouchInput { longClick() }
        compose.onNodeWithText("删除").assertExists()
        compose.onAllNodesWithText("复制").assertCountEquals(0)
        compose.onNodeWithText("删除").performClick()
        compose.onNodeWithText("取消").performClick()
        compose.onNodeWithContentDescription("打开图片").performTouchInput { longClick() }
        compose.onNodeWithText("删除").assertExists()
        compose.onAllNodesWithText("复制").assertCountEquals(0)
    }

    @Test fun pageModelOffersCopyByRecord() {
        val state = AppState()
        assertTrue(record("Text", text = "x").toUi(state, emptyMap()).copyable)
        assertTrue(record("Image", path = downloadedImage().path, mime = "image/png").toUi(state, emptyMap()).copyable)
        assertFalse(record("Image", download = "Downloading").toUi(state, emptyMap()).copyable)
        assertFalse(record("File", path = downloadedImage().path).toUi(state, emptyMap()).copyable)
    }

    @Test fun deleteStillAsksFirst() {
        page()
        compose.onNodeWithText("可以复制的文字").performTouchInput { longClick() }
        compose.onNodeWithText("删除").performClick()
        compose.onNodeWithText("删除这条消息？").assertExists()
        assertTrue(deleted.isEmpty())
        compose.onNodeWithText("删除").performClick()
        compose.waitForIdle()
        assertEquals(listOf(1L), deleted)
        assertTrue(copied.isEmpty())
    }

    // 未展开的聚合长按不能复制 (user 2026-11-12): a collapsed stack's menu has no 复制 in any state;
    // an expanded batch's items keep their own.

    private fun batchItem(id: Long, kind: String, copyable: Boolean = false, progress: Int? = null) =
        MessageUi(id, false, "pc", "Windows PC", "windows", true, kind, name = "项$id", batch = "windowsPc:s1", copyable = copyable, progress = progress)

    @Test fun collapsedStackNeverOffersCopy() {
        for (top in listOf(batchItem(1, "image", copyable = true), batchItem(1, "image", progress = 40), batchItem(1, "file"), batchItem(1, "folder"))) {
            assertFalse(offersCopy(top, collapsedStack = true))
        }
        assertTrue(offersCopy(batchItem(1, "image", copyable = true), collapsedStack = false))
        assertFalse(offersCopy(batchItem(1, "image", progress = 40), collapsedStack = false))
        assertFalse(offersCopy(batchItem(1, "file"), collapsedStack = false))
    }

    private fun batchPage(expanded: Set<String>) = compose.setContent {
        OinkApp(AppModel(messages = listOf(batchItem(1, "image", copyable = true), batchItem(2, "file"))),
            UiInit(tab = Tab.Transfer, transferExpanded = expanded), Prefs(scheme = "light"), {},
            AppActions(copyMessage = { copied += it.id; true }, deleteMessage = { deleted += it.id }), false)
    }

    @Test fun collapsedStackMenuHasNoCopyEvenWithADownloadedImageOnTop() {
        batchPage(emptySet())
        compose.onNodeWithContentDescription("2 项，当前第 1 项", substring = true).performTouchInput { longClick() }
        compose.onNodeWithText("删除").assertExists()
        compose.onAllNodesWithText("复制").assertCountEquals(0)
    }

    @Test fun expandedImageItemKeepsItsCopy() {
        batchPage(setOf("windowsPc:s1"))
        compose.onNodeWithContentDescription("打开图片").performTouchInput { longClick() }
        compose.onNodeWithText("复制").performClick()
        compose.waitForIdle()
        assertEquals(listOf(1L), copied)
    }
}
