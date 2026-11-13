package com.hachiwii.oink

import android.content.ContentResolver
import android.content.Context
import android.content.Intent
import android.content.ClipDescription
import android.os.Build
import android.graphics.Bitmap
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.OpenableColumns
import androidx.core.content.IntentCompat
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import java.io.File
import java.io.InputStream
import java.util.Locale
import java.util.UUID

object ShareStaging {
    /** Metadata only: a missing size stays unknown and always asks the user to choose. */
    fun knownBytes(context: Context, intent: Intent): Long? = runCatching {
        var total = sharedText(intent).toByteArray().size.toLong()
        total = Math.addExact(total, sharedHtml(intent)?.toByteArray()?.size?.toLong() ?: 0L)
        for (uri in streams(intent)) {
            if (context.contentResolver.getType(uri) == DocumentsContract.Document.MIME_TYPE_DIR) {
                total = Math.addExact(total, directoryBytes(context.contentResolver, uri) ?: return null)
                continue
            }
            val size = context.contentResolver.query(uri, arrayOf(OpenableColumns.SIZE), null, null, null)?.use {
                if (it.moveToFirst() && !it.isNull(0)) it.getLong(0).takeIf { n -> n >= 0 } else null
            } ?: return null
            total = Math.addExact(total, size)
        }
        total
    }.getOrNull()

    private fun directoryBytes(resolver: ContentResolver, root: Uri): Long? = runCatching {
        val tree = DocumentsContract.isTreeUri(root)
        val first = if (tree && !root.pathSegments.contains("document")) DocumentsContract.getTreeDocumentId(root) else DocumentsContract.getDocumentId(root)
        val pending = ArrayDeque<String>().apply { add(first) }
        val visited = mutableSetOf<String>()
        var total = 0L
        while (pending.isNotEmpty()) {
            val id = pending.removeFirst()
            if (!visited.add(id)) return null
            val children = if (tree) DocumentsContract.buildChildDocumentsUriUsingTree(root, id)
                else DocumentsContract.buildChildDocumentsUri(root.authority, id)
            val cursor = resolver.query(children, arrayOf(DocumentsContract.Document.COLUMN_DOCUMENT_ID, DocumentsContract.Document.COLUMN_MIME_TYPE, DocumentsContract.Document.COLUMN_SIZE), null, null, null) ?: return null
            cursor.use { rows ->
                if (rows.extras?.getBoolean(DocumentsContract.EXTRA_LOADING) == true) return null
                while (rows.moveToNext()) {
                    if (rows.getString(1) == DocumentsContract.Document.MIME_TYPE_DIR) pending.add(rows.getString(0))
                    else {
                        if (rows.isNull(2) || rows.getLong(2) < 0) return null
                        total = Math.addExact(total, rows.getLong(2))
                    }
                }
            }
        }
        total
    }.getOrNull()
    fun clearAbandoned(context: Context) {
        // A new process never resumes a previous one-shot delivery.
        File(context.cacheDir, "outgoing").deleteRecursively()
    }
    /**
     * What a share intent holds in a few words, for the device picker's header (「3 张照片」, one
     * file's name, 「3 个文件」, or 「文字」), read before anything is copied.
     */
    fun summary(context: Context, intent: Intent?): String {
        if (intent == null) return ""
        val uris = streams(intent)
        val names = uris.mapIndexed { i, uri ->
            runCatching { context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { if (it.moveToFirst()) it.getString(0) else null } }.getOrNull()
                ?: "file-$i"
        }
        val images = uris.count { uri -> (runCatching { context.contentResolver.getType(uri) }.getOrNull() ?: intent.type.orEmpty()).startsWith("image/") }
        return com.hachiwii.oink.ui.itemsSummary(names, images)
    }
    private fun streams(intent: Intent): List<Uri> {
        val streams = when (intent.action) {
            Intent.ACTION_VIEW -> listOfNotNull(intent.data)
            Intent.ACTION_SEND -> listOfNotNull(IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java))
            Intent.ACTION_SEND_MULTIPLE -> IntentCompat.getParcelableArrayListExtra(intent, Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
            else -> emptyList()
        }
        val clipUris = intent.clipData?.let { clip -> (0 until clip.itemCount).mapNotNull { clip.getItemAt(it).uri } }.orEmpty()
        return (streams + clipUris).distinct()
    }
    private fun sharedText(intent: Intent): String =
        intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString()
            ?: intent.getCharSequenceArrayListExtra(Intent.EXTRA_TEXT)?.joinToString("\n")
            ?: intent.clipData?.let { clip -> (0 until clip.itemCount).mapNotNull { clip.getItemAt(it).text?.toString() }.distinct().joinToString("\n") }.orEmpty()
    suspend fun fromIntent(context: Context, intent: Intent): ShareDraft {
        val text = sharedText(intent)
        val sensitive = Build.VERSION.SDK_INT >= 33 && intent.clipData?.description?.extras?.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE) == true
        val uris = streams(intent)
        // A share without readable text or streams must fail visibly instead of replacing the draft with nothing.
        require(text.isNotEmpty() || uris.isNotEmpty()) { "分享内容为空或不受支持" }
        return copy(context, uris, text).copy(sensitive = sensitive, html = sharedHtml(intent).takeIf { text.isNotEmpty() })
    }
    /**
     * The shared text's rich text (user 2026-11-12: every format is sent): `EXTRA_HTML_TEXT`, else the clip
     * items' HTML when every text item has one (so the HTML says what the text says).
     */
    internal fun sharedHtml(intent: Intent): String? {
        intent.getStringExtra(Intent.EXTRA_HTML_TEXT)?.takeIf { it.isNotEmpty() }?.let { return it }
        val clip = intent.clipData ?: return null
        val texts = (0 until clip.itemCount).map(clip::getItemAt).filter { !it.text.isNullOrEmpty() }
        return texts.map { it.htmlText }.takeIf { texts.isNotEmpty() && it.all { html -> !html.isNullOrEmpty() } }
            ?.joinToString("<br>")
    }
    suspend fun copy(context: Context, uris: List<Uri>, text: String = ""): ShareDraft = withContext(Dispatchers.IO) {
        if (uris.isEmpty()) return@withContext ShareDraft(text)
        val directory = File(context.cacheDir, "outgoing/${UUID.randomUUID()}").apply { mkdirs() }
        try {
            val items = uris.flatMapIndexed { index, uri ->
                require(uri.scheme == "content") { "只能读取获授权的内容 URI" }
                val name = context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
                    if (it.moveToFirst()) it.getString(0) else null
                } ?: "file-$index"
                val itemDirectory = File(directory, index.toString()).apply { mkdirs() }
                val file = File(itemDirectory, safeName(name, "file-$index"))
                copyStream(requireNotNull(context.contentResolver.openInputStream(uri)) { "无法读取 $name" }, file)
                val mime = context.contentResolver.getType(uri) ?: "application/octet-stream"
                // JPEG/HEIC/... go as one Image item with their real mime; the proxy publishes
                // file + bitmap like a local copy (decision 38), so no PNG duplicate here.
                listOf(LocalItem(file.path, name, mime, file.length()))
            }
            ShareDraft(text, items)
        } catch (e: Exception) { directory.deleteRecursively(); throw e }
    }
    /** A folder picked with the system tree picker (composer 「文件夹」), staged as a real directory tree. */
    suspend fun folder(context: Context, tree: Uri): ShareDraft =
        stageFolder(File(context.cacheDir, "outgoing"), SafTree(context.contentResolver, tree))
    /**
     * Copies [tree] into `outgoing/<uuid>/0/<name>` keeping its structure, empty directories included,
     * one buffered stream per file (nothing is held in memory). The draft holds that directory as its
     * only item; [RustCoreBridge] hands it to the core as a folder, which packages it as a Stored ZIP
     * for a computer or a phone. A failure or cancel removes everything staged so far.
     */
    internal suspend fun stageFolder(outgoing: File, tree: DocumentTree): ShareDraft = withContext(Dispatchers.IO) {
        val directory = File(outgoing, "${UUID.randomUUID()}/0")
        try {
            val name = tree.name(tree.rootId)?.takeIf { it.isNotBlank() } ?: "folder"
            // The staged root follows the entries' rules too (no ':' in any staged name); the
            // display name the core gets stays as the provider gave it.
            val root = File(directory, entryName(name, "folder"))
            check(root.mkdirs()) { "无法创建暂存目录" }
            var bytes = 0L
            val pending = ArrayDeque(listOf(tree.rootId to root))
            while (pending.isNotEmpty()) {
                val (id, into) = pending.removeLast()
                val taken = HashSet<String>()
                tree.children(id).forEachIndexed { index, child ->
                    currentCoroutineContext().ensureActive()
                    val target = File(into, freeName(taken, entryName(child.name, if (child.directory) "folder-$index" else "file-$index")))
                    if (child.directory) {
                        check(target.mkdir()) { "无法创建暂存目录" }
                        pending.addLast(child.id to target)
                    } else {
                        bytes += copyStream(requireNotNull(tree.open(child.id)) { "无法读取 ${child.name}" }, target)
                    }
                }
            }
            ShareDraft(items = listOf(LocalItem(root.path, name, DocumentsContract.Document.MIME_TYPE_DIR, bytes)))
        } catch (e: Exception) { directory.parentFile?.deleteRecursively(); throw e }
    }
    /** A display name as one path component: what follows the last separator; [fallback] when nothing usable is left. */
    private fun safeName(name: String, fallback: String): String =
        name.substringAfterLast('/').substringAfterLast('\\').takeIf { it.isNotBlank() && it != "." && it != ".." } ?: fallback
    /** A name inside a staged folder. The core packages folder entries as ZIP names without '\\' or ':'
     *  (v2_session `archive_folder`; Windows cannot create them), so ':' becomes '_' here. */
    internal fun entryName(name: String, fallback: String): String = safeName(name, fallback).replace(':', '_')
    /** [name], or `name (2).ext`, … when a sibling already has it ignoring case: a provider may list two
     *  children with one name, and the receiving computer refuses names that differ only in case. */
    internal fun freeName(taken: MutableSet<String>, name: String): String {
        val dot = name.lastIndexOf('.').takeIf { it > 0 } ?: name.length
        return (sequenceOf(name) + generateSequence(2) { it + 1 }.map { "${name.take(dot)} ($it)${name.drop(dot)}" })
            .first { taken.add(it.lowercase(Locale.ROOT)) }
    }
    /** Streams [input] into [file] in 256 KiB steps, checking for cancel between them; returns the byte count. */
    private suspend fun copyStream(input: InputStream, file: File): Long = input.use {
        file.outputStream().use { output ->
            val buffer = ByteArray(256 * 1024)
            var total = 0L
            while (true) {
                currentCoroutineContext().ensureActive()
                val count = input.read(buffer)
                if (count < 0) break
                output.write(buffer, 0, count)
                total += count
            }
            total
        }
    }
    /** A camera capture from the transfer composer, staged as a PNG like other picked images. */
    suspend fun bitmap(context: Context, bitmap: Bitmap): ShareDraft = withContext(Dispatchers.IO) {
        val directory = File(context.cacheDir, "outgoing/${UUID.randomUUID()}/0").apply { mkdirs() }
        val name = "photo-${System.currentTimeMillis()}.png"
        val file = File(directory, name)
        try {
            file.outputStream().use { check(bitmap.compress(Bitmap.CompressFormat.PNG, 100, it)) { "无法保存照片" } }
            ShareDraft("", listOf(LocalItem(file.path, name, "image/png", file.length())))
        } catch (e: Exception) { directory.parentFile?.deleteRecursively(); throw e }
    }
    /** Removes the draft's staging directories. Files the core already took over (moved into its
     *  store by a successful send) are no longer in them; what is left is ours (a failed or refused send). */
    fun delete(context: Context, draft: ShareDraft) {
        val root = File(context.cacheDir, "outgoing").canonicalFile
        draft.items.mapNotNull { item ->
            val parent = File(item.path).canonicalFile.parentFile
            when {
                parent?.parentFile == root -> parent
                parent?.parentFile?.parentFile == root -> parent.parentFile
                else -> null
            }
        }.distinct().forEach { it.deleteRecursively() }
    }
}

/** One child of a picked document tree. */
internal data class TreeEntry(val id: String, val name: String, val directory: Boolean)

/** A picked folder as documents: the SAF tree in the app, an in-memory tree in tests. */
internal interface DocumentTree {
    val rootId: String
    fun name(id: String): String?
    fun children(id: String): List<TreeEntry>
    fun open(id: String): InputStream?
}

/** The tree an ACTION_OPEN_DOCUMENT_TREE result grants, read through its provider (SAF has no symlinks). */
internal class SafTree(private val resolver: ContentResolver, private val tree: Uri) : DocumentTree {
    override val rootId: String = DocumentsContract.getTreeDocumentId(tree)
    private fun document(id: String) = DocumentsContract.buildDocumentUriUsingTree(tree, id)
    // The Bundle query: DocumentsProvider only implements that one (the selection form is pre-O).
    override fun name(id: String): String? =
        resolver.query(document(id), arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME), null, null)
            ?.use { if (it.moveToFirst()) it.getString(0) else null }
    override fun children(id: String): List<TreeEntry> {
        val columns = arrayOf(DocumentsContract.Document.COLUMN_DOCUMENT_ID, DocumentsContract.Document.COLUMN_DISPLAY_NAME,
            DocumentsContract.Document.COLUMN_MIME_TYPE)
        val cursor = resolver.query(DocumentsContract.buildChildDocumentsUriUsingTree(tree, id), columns, null, null)
        return requireNotNull(cursor) { "无法读取文件夹" }.use { c ->
            // A provider still fetching the listing (EXTRA_LOADING) would send part of the folder.
            check(c.extras?.getBoolean(DocumentsContract.EXTRA_LOADING) != true) { "文件夹内容还在加载，请稍后重新选择" }
            buildList {
                while (c.moveToNext()) add(TreeEntry(c.getString(0), c.getString(1).orEmpty(), c.getString(2) == DocumentsContract.Document.MIME_TYPE_DIR))
            }
        }
    }
    override fun open(id: String): InputStream? = resolver.openInputStream(document(id))
}
