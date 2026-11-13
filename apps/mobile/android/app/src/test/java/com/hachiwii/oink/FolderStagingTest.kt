package com.hachiwii.oink

import android.Manifest
import android.content.pm.ProviderInfo
import android.database.Cursor
import android.database.MatrixCursor
import android.os.CancellationSignal
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.provider.DocumentsProvider
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import uniffi.oink_mobile_ffi.MobileShareItem
import java.io.File
import java.io.IOException
import java.io.InputStream

/**
 * Composer 「文件夹」: a picked SAF tree is staged as a real directory (structure and empty
 * directories kept, names made safe) and goes to the core as one folder item.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class FolderStagingTest {
    private val context get() = RuntimeEnvironment.getApplication()
    private val outgoing get() = File(context.cacheDir, "outgoing")

    @After fun clean() { outgoing.deleteRecursively() }

    /** A folder or a file (bytes) of an in-memory tree. */
    private class Node(val name: String, val bytes: ByteArray? = null, val children: List<Node> = emptyList())
    private fun dir(name: String, vararg children: Node) = Node(name, null, children.toList())
    private fun file(name: String, text: String) = Node(name, text.toByteArray())

    private class MemoryTree(root: Node) : DocumentTree {
        private val nodes = HashMap<String, Node>()
        init { index("root", root) }
        private fun index(id: String, node: Node) {
            nodes[id] = node
            node.children.forEachIndexed { i, child -> index("$id/$i", child) }
        }
        override val rootId = "root"
        override fun name(id: String) = nodes.getValue(id).name
        override fun children(id: String) = nodes.getValue(id).children.mapIndexed { i, c -> TreeEntry("$id/$i", c.name, c.bytes == null) }
        override fun open(id: String): InputStream? = nodes.getValue(id).bytes?.inputStream()
    }

    /** Relative paths under [root], directories ending in '/', with each file's text. */
    private fun listing(root: File): Map<String, String?> = root.walkTopDown().filter { it != root }.associate {
        val relative = it.relativeTo(root).invariantSeparatorsPath
        if (it.isDirectory) "$relative/" to null else relative to it.readText()
    }

    @Test fun aPickedTreeIsStagedWithItsStructureAndEmptyFolders() {
        val tree = dir("Trip",
            file("notes.txt", "beach"),
            dir("day 1", file("photo.png", "not really a png"), dir("empty")),
            dir("nothing here"))
        val draft = runBlocking { ShareStaging.stageFolder(outgoing, MemoryTree(tree)) }

        val item = draft.items.single()
        assertEquals("Trip", item.name)
        assertEquals(Document.MIME_TYPE_DIR, item.mime)
        assertEquals(("beach" + "not really a png").length.toLong(), item.size)
        val root = File(item.path)
        // outgoing/<uuid>/0/Trip: the layout ShareStaging.delete removes.
        assertEquals(outgoing.canonicalFile, root.canonicalFile.parentFile?.parentFile?.parentFile)
        assertEquals("Trip", root.name)
        assertEquals(mapOf(
            "notes.txt" to "beach",
            "day 1/" to null,
            "day 1/photo.png" to "not really a png",
            "day 1/empty/" to null,
            "nothing here/" to null,
        ), listing(root))

        // The directory goes to the core as a folder, by path and display name.
        val folder = shareItems(draft) { File(it).isDirectory }.single() as MobileShareItem.Folder
        assertEquals(root.path, folder.path)
        assertEquals("Trip", folder.name)

        // After the send (or its failure) the repository removes the whole staging.
        ShareStaging.delete(context, draft)
        assertEquals(emptyList<String>(), outgoing.list().orEmpty().toList())
    }

    @Test fun entryNamesAreMadeSafeAndUniqueIgnoringCase() {
        val tree = dir("My:Folder",
            file("a:b.txt", "1"),
            file("dir\\y.txt", "2"),
            file("..", "3"),
            dir(""),
            file("Same.txt", "4"),
            file("same.txt", "5"))
        val draft = runBlocking { ShareStaging.stageFolder(outgoing, MemoryTree(tree)) }
        val item = draft.items.single()
        // The display name stays as the provider gave it; only staged entries are renamed.
        assertEquals("My:Folder", item.name)
        assertEquals("My_Folder", File(item.path).name)
        assertEquals(mapOf(
            "a_b.txt" to "1",
            "y.txt" to "2",
            "file-2" to "3",
            "folder-3/" to null,
            "Same.txt" to "4",
            "same (2).txt" to "5",
        ), listing(File(item.path)))
    }

    @Test fun freeNameNumbersBeforeTheExtension() {
        val taken = HashSet<String>()
        assertEquals("archive", ShareStaging.freeName(taken, "archive"))
        assertEquals("Archive (2)", ShareStaging.freeName(taken, "Archive"))
        assertEquals(".env", ShareStaging.freeName(taken, ".env"))
        assertEquals(".env (2)", ShareStaging.freeName(taken, ".env"))
        assertEquals("a.tar.gz", ShareStaging.freeName(taken, "a.tar.gz"))
        assertEquals("a.tar (2).gz", ShareStaging.freeName(taken, "a.tar.gz"))
    }

    @Test fun aFailedReadLeavesNothingStaged() {
        val tree = object : DocumentTree {
            override val rootId = "r"
            override fun name(id: String) = "Broken"
            override fun children(id: String) =
                if (id == "r") listOf(TreeEntry("a", "a.txt", false), TreeEntry("b", "b.txt", false)) else emptyList()
            override fun open(id: String): InputStream? = if (id == "a") "ok".byteInputStream() else throw IOException("revoked")
        }
        assertThrows(IOException::class.java) { runBlocking { ShareStaging.stageFolder(outgoing, tree) } }
        assertEquals(emptyList<String>(), outgoing.list().orEmpty().toList())
    }

    @Test fun aCancelledStagingLeavesNothingStaged() = runBlocking {
        lateinit var job: Job
        // An endless file: the job is cancelled while it is being copied.
        val endless = object : InputStream() {
            override fun read() = 0
            override fun read(b: ByteArray, off: Int, len: Int): Int { job.cancel(); return len }
        }
        val tree = object : DocumentTree {
            override val rootId = "r"
            override fun name(id: String) = "Big"
            override fun children(id: String) = if (id == "r") listOf(TreeEntry("f", "big.bin", false)) else emptyList()
            override fun open(id: String): InputStream = endless
        }
        job = launch(Dispatchers.IO, start = CoroutineStart.LAZY) { ShareStaging.stageFolder(outgoing, tree) }
        job.start()
        job.join()
        assertTrue(job.isCancelled)
        assertEquals(emptyList<String>(), outgoing.list().orEmpty().toList())
    }

    // ---- the SAF tree through a DocumentsProvider ----

    /** Documents of [FakeDocuments]: id → (name, bytes or null for a directory). */
    private val documents: Map<String, Pair<String, String?>> = mapOf(
        "Trip" to ("Trip" to null),
        "Trip/notes.txt" to ("notes.txt" to "beach"),
        "Trip/day 1" to ("day 1" to null),
        "Trip/day 1/empty" to ("empty" to null),
        "Trip/day 1/photo.png" to ("photo.png" to "png"),
    )

    class FakeDocuments : DocumentsProvider() {
        lateinit var documents: Map<String, Pair<String, String?>>
        lateinit var files: File
        override fun onCreate() = true
        private fun row(cursor: MatrixCursor, id: String) {
            val (name, text) = documents.getValue(id)
            cursor.addRow(cursor.columnNames.map { column ->
                when (column) {
                    Document.COLUMN_DOCUMENT_ID -> id
                    Document.COLUMN_DISPLAY_NAME -> name
                    Document.COLUMN_MIME_TYPE -> if (text == null) Document.MIME_TYPE_DIR else "application/octet-stream"
                    Document.COLUMN_SIZE -> text?.length
                    else -> null
                }
            })
        }
        override fun queryRoots(projection: Array<out String>?): Cursor = MatrixCursor(projection ?: arrayOf(DocumentsContract.Root.COLUMN_ROOT_ID))
        override fun queryDocument(documentId: String, projection: Array<out String>?): Cursor =
            MatrixCursor(projection!!).also { row(it, documentId) }
        override fun queryChildDocuments(parentDocumentId: String, projection: Array<out String>?, sortOrder: String?): Cursor =
            MatrixCursor(projection!!).also { cursor ->
                documents.keys.filter { it.substringBeforeLast('/', "") == parentDocumentId }.sorted().forEach { row(cursor, it) }
            }
        override fun isChildDocument(parentDocumentId: String, documentId: String) = documentId.startsWith("$parentDocumentId/")
        override fun openDocument(documentId: String, mode: String, signal: CancellationSignal?): ParcelFileDescriptor {
            val file = File(files, documentId.replace('/', '_')).apply { writeText(documents.getValue(documentId).second!!) }
            return ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY)
        }
    }

    @Test fun aSafTreeIsReadThroughItsDocumentsProvider() {
        val authority = "com.hachiwii.oink.test.documents"
        val provider = Robolectric.buildContentProvider(FakeDocuments::class.java).create(ProviderInfo().apply {
            this.authority = authority
            exported = true
            grantUriPermissions = true
            readPermission = Manifest.permission.MANAGE_DOCUMENTS
            writePermission = Manifest.permission.MANAGE_DOCUMENTS
        }).get()
        provider.documents = documents
        provider.files = File(context.filesDir, "fake-documents").apply { mkdirs() }

        val tree = DocumentsContract.buildTreeDocumentUri(authority, "Trip")
        val draft = runBlocking { ShareStaging.folder(context, tree) }
        val item = draft.items.single()
        assertEquals("Trip", item.name)
        assertEquals(mapOf(
            "notes.txt" to "beach",
            "day 1/" to null,
            "day 1/empty/" to null,
            "day 1/photo.png" to "png",
        ), listing(File(item.path)))
        ShareStaging.delete(context, draft)
        assertEquals(emptyList<String>(), outgoing.list().orEmpty().toList())
    }
}
