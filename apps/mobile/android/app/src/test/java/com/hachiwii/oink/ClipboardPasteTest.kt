package com.hachiwii.oink

import android.content.ClipData
import android.content.ClipDescription
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.PersistableBundle
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import uniffi.oink_mobile_ffi.CardOs
import uniffi.oink_mobile_ffi.TransferOnlyDevice
import uniffi.oink_mobile_ffi.TransferTarget
import uniffi.oink_mobile_ffi.TransferTargetKind
import uniffi.oink_mobile_ffi.TransferTargetsView

/** What the 粘贴 tile reads and where it sends it, and the requests tiles hand to Oink (user 2026-11-11). */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class ClipboardPasteTest {
    private val context = RuntimeEnvironment.getApplication()
    private val image = Uri.parse("content://media/external/images/1")
    private val pdf = Uri.parse("content://downloads/2")
    private val types = mapOf(image to "image/png", pdf to "application/pdf")

    @Test fun sensitiveImageMetadataIsKeptForTheProxy() {
        val uri = android.net.Uri.parse("content://fixtures/image")
        val clip = ClipData(ClipDescription("fixture", arrayOf("image/png")), ClipData.Item(uri))
        clip.description.extras = PersistableBundle().apply { putBoolean(ClipDescription.EXTRA_IS_SENSITIVE, true) }
        val classified = classifyClip(clip) { "image/png" } as ClipPaste.Images
        assertTrue(classified.sensitive)
        assertEquals(listOf(uri), classified.uris)
    }

    @Test fun theShareSizeIncludesClipTextAndEveryRichFormat() {
        val intent = Intent(Intent.ACTION_SEND).apply {
            clipData = ClipData.newHtmlText("fixture", "汉", "<b>汉</b>")
        }
        assertEquals(("汉".toByteArray().size + "<b>汉</b>".toByteArray().size).toLong(), ShareStaging.knownBytes(context, intent))
    }

    @Test fun plainTextIsSentAsText() {
        assertEquals(ClipPaste.Text("hello"), classifyClip(ClipData.newPlainText("x", "hello"), types::get))
    }

    @Test fun aPasswordMarkedTextKeepsItsMark() {
        val clip = ClipData.newPlainText("x", "secret")
        clip.description.extras = PersistableBundle().apply { putBoolean(ClipDescription.EXTRA_IS_SENSITIVE, true) }
        assertEquals(ClipPaste.Text("secret", sensitive = true), classifyClip(clip, types::get))
    }

    /** User 2026-11-12 (later decision): with plain text and rich text on the clipboard, both are sent. */
    @Test fun richTextWithPlainTextSendsBoth() {
        val html = ClipData(ClipDescription("x", arrayOf("text/html", "text/plain")), ClipData.Item("plain", "<b>plain</b>"))
        assertEquals(ClipPaste.Text("plain", html = "<b>plain</b>"), classifyClip(html, types::get))
        // Several items: their texts and their HTML, each joined in order.
        val two = ClipData.newHtmlText("x", "one", "<b>one</b>").apply { addItem(ClipData.Item("two", "<i>two</i>")) }
        assertEquals(ClipPaste.Text("one\ntwo", html = "<b>one</b><br><i>two</i>"), classifyClip(two, types::get))
        // An item without HTML: the plain text alone, so the rich text never says less.
        val mixed = ClipData.newHtmlText("x", "one", "<b>one</b>").apply { addItem(ClipData.Item("two")) }
        assertEquals(ClipPaste.Text("one\ntwo"), classifyClip(mixed, types::get))
        // Styled text (spans) goes as its characters only.
        val styled = android.text.SpannableString("styled").apply {
            setSpan(android.text.style.StyleSpan(android.graphics.Typeface.BOLD), 0, 6, 0)
        }
        assertEquals(ClipPaste.Text("styled"), classifyClip(ClipData.newPlainText("x", styled), types::get))
    }

    /** A share intent's HTML (EXTRA_HTML_TEXT, else its clip items' HTML) is kept with the text. */
    @Test fun aSharedTextKeepsItsHtml() {
        val extra = Intent(Intent.ACTION_SEND).putExtra(Intent.EXTRA_TEXT, "hi").putExtra(Intent.EXTRA_HTML_TEXT, "<b>hi</b>")
        assertEquals("<b>hi</b>", ShareStaging.sharedHtml(extra))
        val clip = Intent(Intent.ACTION_SEND).apply { clipData = ClipData.newHtmlText("x", "hi", "<i>hi</i>") }
        assertEquals("<i>hi</i>", ShareStaging.sharedHtml(clip))
        assertNull(ShareStaging.sharedHtml(Intent(Intent.ACTION_SEND).putExtra(Intent.EXTRA_TEXT, "hi")))
    }

    @Test fun imagesAreSentAsImages() {
        val clip = ClipData(ClipDescription("x", arrayOf("image/png")), ClipData.Item(image))
        assertEquals(ClipPaste.Images(listOf(image)), classifyClip(clip, types::get))
    }

    @Test fun otherFilesIntentsAndNothingArePrompts() {
        assertEquals(ClipPaste.Unsupported, classifyClip(ClipData(ClipDescription("x", arrayOf("application/pdf")), ClipData.Item(pdf)), types::get))
        // An image together with another file: not the transfer page's content either.
        val mixed = ClipData(ClipDescription("x", arrayOf("image/png")), ClipData.Item(image)).apply { addItem(ClipData.Item(pdf)) }
        assertEquals(ClipPaste.Unsupported, classifyClip(mixed, types::get))
        assertEquals(ClipPaste.Unsupported, classifyClip(ClipData.newIntent("x", Intent(Intent.ACTION_VIEW)), types::get))
        // HTML always comes with its plain text; both are sent.
        assertEquals(ClipPaste.Text("bold", html = "<b>bold</b>"), classifyClip(ClipData.newHtmlText("x", "bold", "<b>bold</b>"), types::get))
        assertEquals(ClipPaste.Empty, classifyClip(null, types::get))
        assertEquals(ClipPaste.Empty, classifyClip(ClipData.newPlainText("x", ""), types::get))
    }

    private fun target(id: String, name: String, kind: TransferTargetKind, os: CardOs, order: Long) =
        TransferTarget(id, name, kind, os, false, order.toULong())
    private val pc = target("pc", "Windows PC", TransferTargetKind.COMPUTER, CardOs.WINDOWS, 1)
    private val tablet = target("tab", "iPad", TransferTargetKind.MOBILE, CardOs.IOS, 2)

    @Test fun pasteGoesWhereTheTransferPageWouldSend() {
        val view = TransferTargetsView(1uL, listOf(pc, tablet), 2u, null, false, null)
        // No choice yet: computers first.
        assertEquals("pc", pasteTarget(view, emptyList(), null, text = true))
        assertEquals("pc", pasteTarget(view, emptyList(), null, text = false))
        // The phone chosen last gets images; text is broadcast, so no phone is passed for it.
        assertEquals("tab", pasteTarget(view, emptyList(), "tab", text = false))
        assertNull(pasteTarget(view, emptyList(), "tab", text = true))
        // The chosen device went offline: the next online one.
        assertEquals("pc", pasteTarget(TransferTargetsView(2uL, listOf(pc), 2u, null, false, null), emptyList(), "tab", text = false))
        // The only other device, offline: it stays the target (the send then waits or fails like on the page).
        val only = TransferTargetsView(3uL, emptyList(), 1u, TransferOnlyDevice("pc", "Windows PC", TransferTargetKind.COMPUTER, CardOs.WINDOWS, false), false, null)
        assertEquals("pc", pasteTarget(only, emptyList(), null, text = false))
        assertNull(pasteTarget(only, emptyList(), null, text = true))
        assertNull(pasteTarget(null, emptyList(), "pc", text = false))
    }

    @Test fun onlyTheHelpersTextAnswerIsPastedInTheBackground() {
        fun answer(status: String, text: String? = null, sensitive: Boolean = false) = Bundle().apply {
            putString("status", status); text?.let { putString("text", it) }; putBoolean("sensitive", sensitive)
        }
        assertEquals(ClipPaste.Text("hi"), pasteTextOf(answer("text", "hi")))
        assertEquals(ClipPaste.Text("pw", sensitive = true), pasteTextOf(answer("text", "pw", sensitive = true)))
        // The helper's HTML of the clip goes with its text.
        assertEquals(ClipPaste.Text("hi", html = "<b>hi</b>"),
            pasteTextOf(answer("text", "hi").apply { putString("html", "<b>hi</b>") }))
        // An image or other clip, too large, locked, inactive helper, no helper: open 传输 instead.
        listOf("unreadable", "too_large", "locked", "owner_inactive").forEach { assertNull(pasteTextOf(answer(it))) }
        assertNull(pasteTextOf(answer("text", "")))
        assertNull(pasteTextOf(null))
    }

    @Test fun aLaunchRequestSurvivesItsIntent() {
        val request = LaunchRequest(tab = HomeTab.DEVICES, microphone = false, id = "r1")
        val intent = mainLaunchIntent(context, request)
        assertEquals(request, LaunchRequest.from(intent))
        // false is an explicit state, not a missing one.
        assertEquals(false, LaunchRequest.from(intent)?.microphone)
        assertNull(LaunchRequest.from(intent)?.speaker)
        assertEquals(LaunchRequest(tab = HomeTab.TRANSFER, sendClipboard = true, id = "r2"),
            LaunchRequest.from(mainLaunchIntent(context, LaunchRequest(tab = HomeTab.TRANSFER, sendClipboard = true, id = "r2"))))
    }

    @Test fun reopeningFromRecentAppsRunsNothingAgain() {
        val intent = mainLaunchIntent(context, LaunchRequest(tab = HomeTab.TRANSFER, sendClipboard = true, id = "r1"))
            .addFlags(Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY)
        assertNull(LaunchRequest.from(intent))
        assertNull(LaunchRequest.from(Intent(context, MainActivity::class.java)))
    }
}
