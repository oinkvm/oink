package com.hachiwii.oink

import com.hachiwii.oink.ui.*
import org.junit.Assert.*
import org.junit.Test
import uniffi.oink_mobile_ffi.MobileDownloadStatus

/**
 * File cards (native-rewrite.md 「文件卡片」「图片尺寸」「发送状态行「已发送」」「文件夹页」): what each
 * card's button and second line show, which message says 已发送, and how the core's records map.
 */
class FileCardTest {
    private fun received(id: Long = 1, kind: String = "file", progress: Int? = null, direct: Boolean = false, ended: String? = null,
                         failure: String? = null, unavailable: String? = null) =
        MessageUi(id, false, "windowsPc", "Windows PC", "windows", true, kind, name = "设计说明.pdf", detail = "PDF · 2.4 MB", progress = progress,
            direct = direct, ended = ended, downloadFailure = failure, unavailable = unavailable)
    private fun own(id: Long, kind: String = "file", sent: Boolean = true, ended: String? = null, failure: String? = null, batch: String? = null) =
        MessageUi(id, true, "self", "Phone", "android", true, kind, name = "f$id", sent = sent, ended = ended, failure = failure, batch = batch,
            progress = if (sent) null else 40)

    // ---- the right-hand button ----

    @Test fun receivedFilesDownloadThenShowProgressThenACheck() {
        assertEquals(FileLook(FileSlot.Download), fileLook(received()))
        assertEquals(FileLook(FileSlot.Progress, 45), fileLook(received(progress = 45)))
        assertEquals(FileLook(FileSlot.Done), fileLook(received(progress = 100)))
    }

    @Test fun aFailedDownloadOffersRetryAndSaysSoInRed() {
        val look = fileLook(received(failure = "下载失败"))
        assertEquals(FileSlot.Retry, look.slot)
        assertEquals(appText("下载失败", "Download failed"), look.reason)
        assertTrue(look.reasonBad)
        assertFalse(look.grey)
    }

    @Test fun aFailedPhoneToPhoneReceiveCannotBeRetried() {
        val look = fileLook(received(direct = true, progress = 62, ended = "failed"))
        assertEquals(FileSlot.Failed, look.slot)
        assertTrue(look.grey)
        // Still arriving: the ring; arrived: a check.
        assertEquals(FileLook(FileSlot.Progress, 30), fileLook(received(direct = true, progress = 30)))
        assertEquals(FileLook(FileSlot.Done), fileLook(received(direct = true, progress = 100)))
        // Cancelled: no button, grey tile (the line under the card says 已取消).
        assertEquals(FileLook(null, grey = true), fileLook(received(direct = true, progress = 30, ended = "cancelled")))
    }

    @Test fun anOfflineSourceGreysTheArrowAndSaysWhy() {
        val look = fileLook(received(unavailable = "offline"))
        assertEquals(FileSlot.Offline, look.slot)
        assertEquals(appText("暂时无法加载", "Unavailable for now"), look.reason)
        assertFalse(look.reasonBad)
        // The type colour stays.
        assertFalse(look.grey)
    }

    @Test fun expiredContentHasNoButtonAndIgnoresTaps() {
        val look = fileLook(received(unavailable = "expired"))
        assertNull(look.slot)
        assertEquals(appText("已失效", "Expired"), look.reason)
        assertTrue(look.grey)
        assertTrue(look.inert)
    }

    @Test fun ownFilesAndFoldersHaveNoButton() {
        assertEquals(FileLook(null), fileLook(own(1)))
        // A failed own send keeps its colours; the status line says why.
        assertEquals(FileLook(null), fileLook(own(2, ended = "failed")))
        assertEquals(FileLook(null), fileLook(received(kind = "folder")))
        // A folder only says why it cannot be opened.
        assertEquals(appText("暂时无法加载", "Unavailable for now"), fileLook(received(kind = "folder", unavailable = "offline")).reason)
        assertNull(fileLook(received(kind = "folder", unavailable = "offline")).slot)
        // A folder arriving straight from a phone shows its ring meanwhile, nothing once it is here.
        assertEquals(FileSlot.Progress, fileLook(received(kind = "folder", direct = true, progress = 20)).slot)
        assertNull(fileLook(received(kind = "folder", direct = true, progress = 100)).slot)
    }

    @Test fun folderRowsUseTheSameButton() {
        assertEquals(FileLook(FileSlot.Download), fileLook(FolderItemUi("1", "image", "a.jpg", "2 MB")))
        assertEquals(FileLook(FileSlot.Progress, 40), fileLook(FolderItemUi("2", "file", "b.pdf", "1 MB", progress = 40)))
        assertEquals(FileLook(FileSlot.Done), fileLook(FolderItemUi("3", "file", "c.xlsx", "88 KB", progress = 100)))
        assertEquals(FileSlot.Retry, fileLook(FolderItemUi("4", "file", "d.zip", "486 MB", failure = "下载失败")).slot)
        // Subfolders: a chevron instead.
        assertNull(fileLook(FolderItemUi("5", "folder", "图纸", "")).slot)
    }

    @Test fun theSecondLineShowsTheSizeBesideTheExtension() {
        assertEquals("2.4 MB", cardSize(received()))
        assertEquals("PDF", fileExt(received()))
        // Size unknown: nothing after the extension.
        assertEquals("", cardSize(received().copy(detail = "PDF")))
        // Folders: their file count.
        assertEquals("12 个文件", cardSize(received(kind = "folder").copy(name = "项目素材", detail = "12 个文件")))
        assertEquals("", fileExt(received(kind = "folder")))
        // Type icons by extension; pictures in a folder use the plain file icon.
        assertEquals(DesignIconData.lucideFileSpreadsheet, fileGlyph("file", "排期表.xlsx"))
        assertEquals(DesignIconData.lucideFileArchive, fileGlyph("file", "原始照片.zip"))
        assertEquals(DesignIconData.lucideFile, fileGlyph("image", "现场 01.jpg"))
        assertEquals(DesignIconData.lucideFolder, fileGlyph("folder", "图纸"))
    }

    // ---- 已发送 ----

    @Test fun pcAndGroupMessagesHaveNoSuccessCaption() {
        assertFalse(own(1).showsSuccessCaption)
        assertFalse(own(2,"text").showsSuccessCaption)
    }
    @Test fun everyDirectSuccessKeepsItsRecipientWhileNewerMessagesSendOrFail() {
        val first=own(1).copy(direct=true,toName="Phone")
        assertTrue(first.showsSuccessCaption)
        assertTrue(own(2).copy(direct=true,toName="Phone").showsSuccessCaption)
        assertFalse(first.copy(sent=false).showsSuccessCaption)
        assertFalse(first.copy(ended="failed").showsSuccessCaption)
        assertFalse(first.copy(failure="failed").showsSuccessCaption)
        assertTrue(received(3).copy(direct=true,toName="This phone",progress=100).showsSuccessCaption)
    }

    // ---- from the core's records ----

    private fun record(id: Long, kind: String, status: MobileDownloadStatus?, outgoing: Boolean = false, name: String = "a.pdf", localPath: String? = null,
                       thumbnail: String? = null, width: Int? = null, height: Int? = null, node: Long? = null) =
        TransferMessage(id, 1_000, if (outgoing) "self" else "pc", null, "c$id", kind, name, null, 2_400_000, null, "placeholder", localPath, true, ByteArray(0),
            isOutgoing = outgoing, directoryNode = node, thumbnailPath = thumbnail, imageWidth = width, imageHeight = height, downloadStatus = status)
    private fun ui(record: TransferMessage) = record.toUi(AppState(name = "Phone"), emptyMap())

    @Test fun theCoresDownloadStatusDrivesTheCard() {
        assertEquals(FileSlot.Download, fileLook(ui(record(1, "file", MobileDownloadStatus.NOT_DOWNLOADED))).slot)
        assertEquals(FileSlot.Progress, fileLook(ui(record(2, "file", MobileDownloadStatus.DOWNLOADING))).slot)
        assertEquals(FileSlot.Done, fileLook(ui(record(3, "file", MobileDownloadStatus.DOWNLOADED))).slot)
        assertEquals(FileSlot.Retry, fileLook(ui(record(4, "file", MobileDownloadStatus.DOWNLOAD_FAILED))).slot)
        assertEquals(FileSlot.Offline, fileLook(ui(record(5, "file", MobileDownloadStatus.SOURCE_OFFLINE))).slot)
        assertTrue(fileLook(ui(record(6, "file", MobileDownloadStatus.EXPIRED))).inert)
        assertEquals("failed", ui(record(7, "file", MobileDownloadStatus.RECEIVE_FAILED)).ended)
        assertEquals("cancelled", ui(record(8, "file", MobileDownloadStatus.CANCELLED)).ended)
    }

    @Test fun picturesTakeTheirAspectFromTheCoresSize() {
        // Own: the original's size as sent; received: the thumbnail's.
        assertEquals(3f / 4f, ui(record(1, "image", null, outgoing = true, width = 3024, height = 4032)).aspect)
        assertEquals(16f / 10f, ui(record(2, "image", MobileDownloadStatus.NOT_DOWNLOADED, width = 512, height = 320)).aspect)
        assertNull(ui(record(3, "image", MobileDownloadStatus.NOT_DOWNLOADED)).aspect)
    }

    @Test fun folderRowsFollowTheirChildRecord() {
        val entry = DirectoryEntry(7, 1, "现场 01.jpg", "file", 3_200_000, 1)
        // No child yet: not downloaded, the type tile.
        folderItem(entry, null, null).let { assertEquals("image", it.kind); assertNull(it.progress); assertNull(it.path); assertNull(it.thumbnailPath) }
        // A child made for the thumbnail is not a download.
        folderItem(entry, record(20, "image", MobileDownloadStatus.NOT_DOWNLOADED, name = entry.name, thumbnail = "/t/7.jpg", node = 7), null).let {
            assertEquals("/t/7.jpg", it.thumbnailPath); assertNull(it.progress); assertNull(it.path)
        }
        folderItem(entry, record(20, "image", MobileDownloadStatus.DOWNLOADING, name = entry.name, node = 7), 40).let { assertEquals(40, it.progress) }
        folderItem(entry, record(20, "image", MobileDownloadStatus.DOWNLOADED, name = entry.name, localPath = "/d/现场 01.jpg", node = 7), null).let {
            assertEquals(100, it.progress); assertEquals("/d/现场 01.jpg", it.path)
        }
        assertEquals("下载失败", folderItem(entry, record(20, "image", MobileDownloadStatus.DOWNLOAD_FAILED, name = entry.name, node = 7), null).failure)
        assertEquals("folder", folderItem(DirectoryEntry(8, 1, "图纸", "directory", null, 1), null, null).kind)
    }
}
