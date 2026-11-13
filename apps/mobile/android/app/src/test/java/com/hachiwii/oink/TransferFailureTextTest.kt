package com.hachiwii.oink

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import uniffi.oink_mobile_ffi.MobileException
import uniffi.oink_mobile_ffi.TransferFailure

/** M18: the host shows the typed reason the core reports, never a generic "connection lost". */
class TransferFailureTextTest {
    @Test
    fun sourceReasonsStayDistinct() {
        val texts = listOf(
            TransferFailure.SOURCE_OFFLINE, TransferFailure.EVICTED, TransferFailure.NO_SUCH_ITEM,
            TransferFailure.SUPERSEDED, TransferFailure.EXPIRED, TransferFailure.NO_TARGET, TransferFailure.LINK_LOST,
        ).map(::transferFailureText)
        assertEquals(texts.size, texts.toSet().size)
        assertEquals("没有在线的电脑", transferFailureText(TransferFailure.NO_TARGET))
    }

    @Test
    fun typedTransferErrorsUseTheirReasonText() {
        val error = MobileException.Transfer(TransferFailure.SUPERSEDED, "content source unavailable: Superseded")
        assertEquals(transferFailureText(TransferFailure.SUPERSEDED), error.oinkMessage)
    }

    @Test
    fun aFailedFolderListingSaysGoneOrWhyItCouldNotBeRead() {
        assertEquals("文件夹内容已失效", folderFailureText(TransferFailure.EVICTED))
        assertEquals("文件夹内容已失效", folderFailureText(TransferFailure.NO_SUCH_ITEM))
        assertEquals("无法获取文件夹内容：" + transferFailureText(TransferFailure.SOURCE_OFFLINE),
            folderFailureText(TransferFailure.SOURCE_OFFLINE))
        assertEquals("无法获取文件夹内容：" + transferFailureText(TransferFailure.LINK_LOST), folderFailureText(TransferFailure.LINK_LOST))
    }

    @Test
    fun storedAvailabilityExplainsAnUnavailableRecord() {
        assertNull(availabilityText("Ready"))
        assertEquals(transferFailureText(TransferFailure.NO_SUCH_ITEM), availabilityText("NoSuchItem"))
        assertEquals(transferFailureText(TransferFailure.SOURCE_OFFLINE), availabilityText("SourceOffline"))
    }
}
