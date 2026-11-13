package com.hachiwii.oink

import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.webkit.MimeTypeMap
import androidx.core.content.FileProvider
import com.hachiwii.oink.privilege.ClipMarker
import java.io.File
import java.util.Locale

object ClipboardSink {
    fun uri(context: Context, item: LocalItem): Uri = uri(context, File(item.path))
    fun uri(context: Context, file: File): Uri = FileProvider.getUriForFile(context, "${context.packageName}.received", file)
    fun copy(context: Context, content: ReceivedContent) {
        if (content.items.isEmpty() && content.text == null) return
        context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clip(context, content))
    }
    /** The clip Oink writes for received content, with its clip marker (see [ClipMarker]). Text with rich
     * text is one HTML clip: its plain text plus its HTML (user 2026-11-12). */
    fun clip(context: Context, content: ReceivedContent): ClipData {
        val clip = if (content.items.isNotEmpty()) {
            ClipData(ClipDescription("Oink", (content.items.map { it.mime } + if (content.text != null) listOf("text/plain") else emptyList()).distinct().toTypedArray()),
                ClipData.Item(uri(context, content.items.first()))).also { data ->
                content.items.drop(1).forEach { data.addItem(ClipData.Item(uri(context, it))) }
                content.text?.let { data.addItem(ClipData.Item(it)) }
            }
        } else textClip(content.text ?: error("no content to copy"), content.html)
        ClipMarker.describe(clip, content.marker, content.sensitive)
        return clip
    }
    /**
     * Plain text, or rich text with its plain text: one item carrying both, described as text/plain and
     * text/html. Not `ClipData.newHtmlText`: it describes the clip as text/html only, so an app that checks
     * `hasMimeType(text/plain)` before offering paste (the platform's own samples do) would see no text.
     * Android has no RTF clip type; the core hands over HTML for RTF.
     */
    fun textClip(text: String, html: String?): ClipData =
        if (html.isNullOrEmpty()) ClipData.newPlainText("Oink", text)
        else ClipData(ClipDescription("Oink", arrayOf(ClipDescription.MIMETYPE_TEXT_PLAIN, ClipDescription.MIMETYPE_TEXT_HTML)), ClipData.Item(text, html))
    /** Transfer page 复制: text, or an image whose data is downloaded. Files, folders and rich text
     * are never copied (they are opened or shared, not pasted). */
    fun canCopy(message: TransferMessage): Boolean = when (message.kind.lowercase(Locale.US)) {
        "text" -> message.text != null
        "image" -> message.downloadState.equals("complete", true) && message.localPath?.let { File(it).isFile } == true
        else -> false
    }
    /**
     * Writes [message] to the clipboard ([clip]); false when it has nothing to copy. The long-press menu's
     * 复制 is [copyRich] (the default), 复制文本 is [copyPlain] (user 2026-11-12).
     */
    fun copy(context: Context, message: TransferMessage, html: String? = null): Boolean {
        val clip = clip(context, message, html = html) ?: return false
        context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clip)
        return true
    }
    /** 复制: a text message with its rich text ([rich], `MobileSession.transferRichText`) and plain text; other
     * messages as [copy]. */
    fun copyRich(context: Context, message: TransferMessage, rich: uniffi.oink_mobile_ffi.MobileRichText?): Boolean =
        copy(context, message, rich?.html)
    /** 复制文本: a text message's plain text only. */
    fun copyPlain(context: Context, message: TransferMessage): Boolean = copy(context, message)
    /**
     * The clip for copying a conversation message: the text (with [html], its rich text, as one HTML
     * clip), or the downloaded image as a content URI under its image type, plus the record's clip marker
     * (none for a direct share), so the PC that announced the clip recognises it when it comes back. Null
     * when there is nothing to copy. [uriOf] is the FileProvider URI; tests on a Windows host replace it,
     * since FileProvider only matches '/'-separated paths.
     */
    fun clip(context: Context, message: TransferMessage, uriOf: (File) -> Uri = { uri(context, it) }, html: String? = null): ClipData? {
        if (!canCopy(message)) return null
        val clip = if (message.kind.equals("text", true)) textClip(message.text ?: return null, html)
        else {
            val file = File(message.localPath ?: return null)
            val mime = imageMime(file.name, message.mime) ?: return null
            ClipData(ClipDescription("Oink", arrayOf(mime)), ClipData.Item(uriOf(file)))
        }
        // A password-marked text keeps its mark: Android 13+ then hides it in the clipboard preview.
        ClipMarker.describe(clip, message.clipMarker, message.sensitive)
        return clip
    }
    /** The downloaded image's type: from the file's extension, else the record's mime type; only image types qualify. */
    fun imageMime(name: String, mime: String?): String? =
        listOfNotNull(MimeTypeMap.getSingleton().getMimeTypeFromExtension(name.substringAfterLast('.', "").lowercase(Locale.US)), mime)
            .firstOrNull { it.startsWith("image/") }
    fun open(context: Context, item: LocalItem) {
        context.startActivity(Intent(Intent.ACTION_VIEW).setDataAndType(uri(context, item), item.mime)
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION))
    }
    fun share(context: Context, content: ReceivedContent) {
        val intent = Intent(if (content.items.size > 1) Intent.ACTION_SEND_MULTIPLE else Intent.ACTION_SEND)
        intent.type = content.items.singleOrNull()?.mime ?: if (content.items.isEmpty()) "text/plain" else "*/*"
        content.text?.let { intent.putExtra(Intent.EXTRA_TEXT, it) }
        if (content.items.isNotEmpty()) {
            val uris = ArrayList(content.items.map { uri(context, it) })
            if (uris.size == 1) intent.putExtra(Intent.EXTRA_STREAM, uris.first())
            else intent.putParcelableArrayListExtra(Intent.EXTRA_STREAM, uris)
            intent.clipData = ClipData.newUri(context.contentResolver, "Oink", uris.first()).apply {
                uris.drop(1).forEach { addItem(ClipData.Item(it)) }
            }
            intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        context.startActivity(Intent.createChooser(intent, "分享收到的内容"))
    }
}
