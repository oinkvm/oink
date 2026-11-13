package com.hachiwii.oink

import android.content.Context
import java.io.File
import uniffi.oink_mobile_ffi.MobileDeliveryFailure
import uniffi.oink_mobile_ffi.MobileDeliveryState
import uniffi.oink_mobile_ffi.MobileDownloadStatus
import uniffi.oink_mobile_ffi.MobileImagePreview
import uniffi.oink_mobile_ffi.MobileMessage
import uniffi.oink_mobile_ffi.MobilePlatform
import uniffi.oink_mobile_ffi.MobileTransferStore

data class TransferMessage(
    val id: Long,
    val createdMs: Long,
    val senderId: String,
    val proxyId: String?,
    val contentId: String,
    val kind: String,
    val displayName: String,
    val mime: String?,
    val size: Long,
    val text: String?,
    val downloadState: String,
    val localPath: String?,
    val sourceOnline: Boolean,
    val operationId: ByteArray,
    /** Right-hand bubble: this phone sent it. */
    val isOutgoing: Boolean = false,
    /** Own messages only: Sending / Sent / Failed, with a typed reason when failed. */
    val deliveryState: MobileDeliveryState? = null,
    val deliveryFailure: MobileDeliveryFailure? = null,
    /** Folder messages: entry count ("N 个文件"); null when unknown. */
    val itemCount: Int? = null,
    /** Set for a file downloaded from inside a folder message (never listed in the conversation). */
    val parentId: Long? = null,
    /** Folder child: the listing node it was downloaded from (`DirectoryEntry.id`). */
    val directoryNode: Long? = null,
    /** "html" / "rtf" for rich text (D15). */
    val richTextFormat: String? = null,
    val sourceAvailability: String = "",
    /** Oink clip marker (`MobileMessage.clipMarker`) written next to the content when the user
     * copies the message; null for a record without a clip identity (a direct share). */
    val clipMarker: ByteArray? = null,
    /** Image messages: what the bubble shows (core `ImagePreview`, same as iOS). */
    val imagePreview: MobileImagePreview? = null,
    /** The PC's preview of a not-downloaded image (JPEG/PNG), and its pixel size. */
    val thumbnailPath: String? = null,
    val thumbnailWidth: Int? = null,
    val thumbnailHeight: Int? = null,
    /**
     * Text copied with a password marker on the computer: hidden on the transfer page until
     * revealed, and copied with ClipDescription.EXTRA_IS_SENSITIVE. From the core's
     * `MobileMessage.sensitive`.
     */
    val sensitive: Boolean = false,
    /**
     * Phone-to-phone (native-rewrite.md 「手机点对点传输」): the other phone, the target of an own
     * message or the sender of a received one (`MobileMessage.direct_peer`); null for broadcast
     * and computer-directed messages. [directPeerName]: the target's name when an own one was sent.
     */
    val directPeer: String? = null,
    val directPeerName: String? = null,
    /**
     * Image messages: the picture's pixel size as shown (`MobileMessage.image_width/height`): the
     * original's for own pictures, the thumbnail's for received ones; null when not known yet.
     */
    val imageWidth: Int? = null,
    val imageHeight: Int? = null,
    /**
     * Received content: what its card shows (`MobileMessage.download_status`): not downloaded,
     * downloading, downloaded, a download that failed and can be retried, a phone-to-phone receive
     * that failed or was cancelled (no retry), source offline, expired. Null for own messages.
     */
    val downloadStatus: MobileDownloadStatus? = null,
    val historyOrder: String? = null,
)

data class TransferPage(val messages: List<TransferMessage>, val nextCreatedMs: Long?, val nextId: Long?)

/**
 * The core-owned transfer history. While a session runs it must be used through the session's own
 * store ([SessionTransferHistory]): that store owns the running downloads and the auto-open slot,
 * so deleting or clearing through a second connection would leave downloads writing to deleted
 * records. [OfflineTransferHistory] is only for when no session runs. Calls block; use Dispatchers.IO.
 */
interface TransferHistory {
    fun sharedBoundaryMs(): Long? = null
    fun hasSharedMore(): Boolean = false
    suspend fun loadSharedOlder() {}
    /** Newest first, as the store pages it. */
    fun page(createdMs: Long? = null, id: Long? = null, limit: Int = 20): TransferPage
    /** One conversation record or folder child; null after deletion. */
    fun message(id: Long): TransferMessage?
    fun folderChildren(folderId: Long): List<TransferMessage>
    /** The rich text kept with text message [id] (user 2026-11-12); null when it has none here. */
    fun richText(id: Long): uniffi.oink_mobile_ffi.MobileRichText? = null
    /** Deletes the record and its downloaded file (a folder also drops its children). */
    fun deleteMessage(messageId: Long)
    fun clearCache()
    fun cachedBytes(): Long
    /** 清理传输记录: deletes every finished record and its files; transfers still moving stay. */
    fun clearHistory()
}

/** Second connection to the same database, for use while no session runs (history at launch,
 * persistent notification off and Oink in the background, session start failures). */
class OfflineTransferHistory(context: Context) : TransferHistory {
    private val root = File(context.filesDir, "core")
    private val opened by lazy { runCatching { root.mkdirs(); MobileTransferStore.openInSession(root.absolutePath, MobilePlatform.ANDROID) } }
    private val store: MobileTransferStore get() = opened.getOrThrow()

    override fun page(createdMs: Long?, id: Long?, limit: Int): TransferPage =
        store.page(createdMs, id, limit.toUInt()).let { page -> TransferPage(page.messages.map { it.toTransferMessage() }, page.nextCreatedMs, page.nextId) }
    override fun message(id: Long): TransferMessage? = store.message(id)?.toTransferMessage()
    override fun folderChildren(folderId: Long): List<TransferMessage> = store.folderChildren(folderId).map { it.toTransferMessage() }
    override fun richText(id: Long): uniffi.oink_mobile_ffi.MobileRichText? = store.richText(id)
    override fun deleteMessage(messageId: Long) { store.deleteMessage(messageId) }
    override fun clearCache() { store.clearCachedFiles() }
    override fun cachedBytes(): Long = store.cachedBytes().toLong()
    override fun clearHistory() { store.clearHistory() }
}

/** The running session's own store (`MobileSession.transfer_*`). */
class SessionTransferHistory(private val session: uniffi.oink_mobile_ffi.MobileSession) : TransferHistory {
    override fun sharedBoundaryMs(): Long? = session.transferHistoryBoundaryMs()
    override fun hasSharedMore(): Boolean = session.transferHistoryHasMore()
    override suspend fun loadSharedOlder() { session.transferLoadOlderHistory() }

    override fun page(createdMs: Long?, id: Long?, limit: Int): TransferPage =
        session.transferPage(createdMs, id, limit.toUInt()).let { page -> TransferPage(page.messages.map { it.toTransferMessage() }, page.nextCreatedMs, page.nextId) }
    override fun message(id: Long): TransferMessage? = session.transferMessage(id)?.toTransferMessage()
    override fun folderChildren(folderId: Long): List<TransferMessage> = session.transferFolderChildren(folderId).map { it.toTransferMessage() }
    override fun richText(id: Long): uniffi.oink_mobile_ffi.MobileRichText? = session.transferRichText(id)
    override fun deleteMessage(messageId: Long) { session.transferDeleteMessage(messageId) }
    override fun clearCache() { session.transferClearCachedFiles() }
    override fun cachedBytes(): Long = session.transferCachedBytes().toLong()
    override fun clearHistory() { session.transferClearHistory() }
}

internal fun MobileMessage.toTransferMessage() = TransferMessage(
    id, createdMs, senderId, proxyId, contentId, kind, displayName, mime, size.toLong(), text,
    downloadState, localPath, sourceOnline, operationId,
    isOutgoing = isOutgoing, deliveryState = deliveryState, deliveryFailure = deliveryFailure,
    itemCount = itemCount?.toInt(), parentId = parentId, directoryNode = directoryNode?.toLong(),
    richTextFormat = richTextFormat, sourceAvailability = sourceAvailability, clipMarker = clipMarker,
    imagePreview = imagePreview, thumbnailPath = thumbnailPath,
    thumbnailWidth = thumbnailWidth?.toInt(), thumbnailHeight = thumbnailHeight?.toInt(),
    sensitive = sensitive,
    directPeer = directPeer, directPeerName = directPeerName,
    imageWidth = imageWidth?.toInt(), imageHeight = imageHeight?.toInt(),
    downloadStatus = downloadStatus,
    historyOrder = historyOrder,
)

/** Conversation order is oldest first (chat bubbles grow downwards); the store pages newest first. */
internal fun mergeTransferPage(current: List<TransferMessage>, page: List<TransferMessage>, reset: Boolean): List<TransferMessage> {
    val older = page.asReversed()
    return (if (reset) older else (older + current).distinctBy { it.id }).sortedWith(compareBy({it.createdMs},{it.historyOrder.orEmpty()},{it.id}))
}

/** Inserts or replaces one record keeping (createdMs, id) ascending; null removes it. */
internal fun upsertTransfer(current: List<TransferMessage>, id: Long, value: TransferMessage?): List<TransferMessage> {
    val rest = current.filterNot { it.id == id }
    if (value == null) return rest
    val index = rest.indexOfFirst { it.createdMs > value.createdMs || (it.createdMs == value.createdMs && (it.historyOrder.orEmpty() > value.historyOrder.orEmpty() || (it.historyOrder == value.historyOrder && it.id > value.id))) }
    return if (index < 0) rest + value else rest.subList(0, index) + value + rest.subList(index, rest.size)
}
