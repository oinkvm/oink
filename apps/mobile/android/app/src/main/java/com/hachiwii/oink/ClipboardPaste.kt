package com.hachiwii.oink

import android.content.ClipData
import android.content.ClipDescription
import android.net.Uri
import android.os.Build
import android.os.Bundle
import com.hachiwii.oink.ui.composerTarget

/** This phone's clipboard as the 粘贴 tile sends it (native-rewrite.md 「控制中心快捷开关」). */
sealed interface ClipPaste {
    /** [html]: the clip's rich text next to its plain text (user 2026-11-12: every format is sent). */
    data class Text(val text: String, val sensitive: Boolean = false, val html: String? = null) : ClipPaste
    /** Image content URIs; reading the clip granted Oink temporary access to them. */
    data class Images(val uris: List<Uri>, val sensitive: Boolean = false) : ClipPaste
    /** Nothing on the clipboard, or nothing readable (no window focus). */
    data object Empty : ClipPaste
    /** Files or other non-image URIs, or an intent only: the transfer page cannot send it as is. (HTML always
     * carries its plain text, which is sent with it.) */
    data object Unsupported : ClipPaste
}

internal const val PASTE_EMPTY = "剪贴板是空的"
internal const val PASTE_UNSUPPORTED = "剪贴板里的内容不能发送"

/**
 * What the transfer page can send from [clip]: images when every URI is an image ([typeOf] resolves
 * the content type), else text: the items' plain text with their HTML (`ClipData.Item.getHtmlText`,
 * user 2026-11-12: the phone sends every format it has). Anything else is [ClipPaste.Unsupported].
 */
internal fun classifyClip(clip: ClipData?, typeOf: (Uri) -> String?): ClipPaste {
    if (clip == null || clip.itemCount == 0) return ClipPaste.Empty
    val sensitive = Build.VERSION.SDK_INT >= 33 &&
        clip.description.extras?.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE) == true
    val items = (0 until clip.itemCount).map(clip::getItemAt)
    val uris = items.mapNotNull { it.uri }
    if (uris.isNotEmpty()) {
        val mime = clip.description.takeIf { it.mimeTypeCount > 0 }?.getMimeType(0)
        val images = uris.filter { (typeOf(it) ?: mime).orEmpty().startsWith("image/") }
        return if (images.size == uris.size) ClipPaste.Images(images, sensitive) else ClipPaste.Unsupported
    }
    val texts = items.filter { !it.text.isNullOrEmpty() }
    if (texts.isNotEmpty()) {
        // Rich text only when every text item has it, so the HTML says what the plain text says.
        val html = texts.map { it.htmlText }.takeIf { all -> all.all { !it.isNullOrEmpty() } }?.joinToString("<br>")
        return ClipPaste.Text(texts.joinToString("\n") { it.text.toString() }, sensitive, html)
    }
    return if (items.any { it.htmlText != null || it.intent != null }) ClipPaste.Unsupported else ClipPaste.Empty
}

/**
 * The device the transfer page would send [text] or attachments to (ui/TransferTarget.kt
 * [composerTarget], user 2026-11-11): the device chosen last ([chosen]) while it is online, else
 * another online one (computers first); the group's only other device even while it is offline (the
 * send then fails or waits like on the page). Text is broadcast whatever the target; only an online
 * computer is passed for it (it carries text too long to broadcast).
 */
internal fun pasteTarget(targets: uniffi.oink_mobile_ffi.TransferTargetsView?, devices: List<Device>, chosen: String?, text: Boolean): String? {
    val ui = targets?.toUi(devices.associateBy { it.id }) ?: return null
    val known = composerTarget(ui, emptyList(), chosen).target ?: return null
    return if (text) known.id.takeIf { !known.mobile && !known.offline } else known.id
}

/**
 * The helper's `clipboard_read` answer as text to paste; null for anything else (an image or other
 * non-text clip, too large, locked, helper inactive), which the 粘贴 tile sends through the transfer
 * page instead.
 */
internal fun pasteTextOf(answer: Bundle?): ClipPaste.Text? {
    if (answer?.getString("status") != "text") return null
    val text = answer.getString("text")?.takeIf { it.isNotEmpty() } ?: return null
    return ClipPaste.Text(text, answer.getBoolean("sensitive"), answer.getString("html")?.takeIf { it.isNotEmpty() })
}

/** Result of copying the group's current shared clipboard to this phone (notification tap, 复制 tile). */
sealed interface CopyOutcome {
    data object Copied : CopyOutcome
    /** Nothing copyable: empty, its source offline, no computer to ask, or a file / folder / rich text. */
    data object Nothing : CopyOutcome
    data class Failed(val message: String) : CopyOutcome
}
