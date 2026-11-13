package com.hachiwii.oink

import android.app.Notification
import android.app.PendingIntent
import android.content.Intent
import androidx.core.app.NotificationCompat
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import uniffi.oink_mobile_ffi.CardOs
import uniffi.oink_mobile_ffi.MobileDeliveryFailure
import uniffi.oink_mobile_ffi.MobileDeliveryState
import uniffi.oink_mobile_ffi.MobileDownloadStatus
import uniffi.oink_mobile_ffi.TransferOnlyDevice
import uniffi.oink_mobile_ffi.TransferTarget
import uniffi.oink_mobile_ffi.TransferTargetKind
import uniffi.oink_mobile_ffi.TransferTargetsView

/** Phone-to-phone host parts: routing to a picked phone, the share card text, and the notifications. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "zh-rCN", application = android.app.Application::class)
class PhoneToPhoneTest {
    private val context = RuntimeEnvironment.getApplication()
    private val self = Device("me", "me", true, isSelf = true, connection = DeviceConnection.THIS_DEVICE)
    private fun phone(id: String, online: Boolean, connection: DeviceConnection? = if (online) DeviceConnection.ONLINE else DeviceConnection.DISCONNECTED) =
        Device(id, "Phone $id", online, computer = false, connection = connection)
    private fun pc(id: String, online: Boolean) = Device(id, "PC $id", online, computer = true, connection = if (online) DeviceConnection.ONLINE else DeviceConnection.DISCONNECTED)
    private val file = ShareDraft(items = listOf(LocalItem("/staged/a.zip", "a.zip", "application/zip", 10)))
    private val smallPng = ShareDraft(items = listOf(LocalItem("/staged/a.png", "a.png", "image/png", 10)))

    @Test fun filesForAPickedPhoneGoToItAlone() {
        assertEquals(SendRoute.Ready("m", "Phone m"), sendRoute(true, listOf(self, pc("p", true), phone("m", true)), "m", file))
        // Even a small picture that would otherwise be broadcast.
        assertEquals(SendRoute.Ready("m", "Phone m"), sendRoute(true, listOf(self, pc("p", true), phone("m", true)), "m", smallPng))
        // Never another device instead.
        assertEquals(SendRoute.Fail("Phone m 不在线"), sendRoute(true, listOf(self, pc("p", true), phone("m", false)), "m", file))
        assertEquals(SendRoute.Wait, sendRoute(true, listOf(self, phone("m", false, DeviceConnection.CONNECTING)), "m", file))
        // Text is still broadcast.
        assertEquals(null, (sendRoute(true, listOf(self, phone("m", true)), "m", ShareDraft("hi")) as SendRoute.Ready).proxy)
    }

    @Test fun theCoresTargetsBecomeTheComposersAndPickersList() {
        fun target(id: String, name: String, kind: TransferTargetKind, os: CardOs, order: Long) =
            TransferTarget(id, name, kind, os, false, order.toULong())
        val view = TransferTargetsView(3uL, listOf(target("pc", "Windows PC", TransferTargetKind.COMPUTER, CardOs.WINDOWS, 2), target("tab", "iPad", TransferTargetKind.MOBILE, CardOs.IOS, 1)),
            4u, null, true, null)
        // The member's own platform names the badge (iPad, not iPhone).
        val ui = view.toUi(mapOf("tab" to Device("tab", "iPad", true, platform = "ipados")))
        assertEquals(listOf("pc" to "windows", "tab" to "ipad"), ui.online.map { it.id to it.platform })
        assertEquals(4, ui.otherDevices)
        assertTrue(ui.searching)
        assertNull(ui.only)
        val only = TransferTargetsView(4uL, emptyList(), 1u, TransferOnlyDevice("m", "Pixel", TransferTargetKind.MOBILE, CardOs.ANDROID, false), false, null).toUi(emptyMap())
        assertEquals("Pixel", only.only?.name)
        assertTrue(only.only!!.offline)
    }

    private fun record(outgoing: Boolean, peer: String?, state: MobileDeliveryState? = null, failure: MobileDeliveryFailure? = null, download: String = "complete",
                       status: MobileDownloadStatus? = if (outgoing) null else receivedStatus(download)) =
        TransferMessage(1, 0, if (outgoing) "self" else "m", null, "c1", "file", "a.zip", null, 10, null, download, null, true, ByteArray(16),
            isOutgoing = outgoing, deliveryState = state, deliveryFailure = failure, directPeer = peer, directPeerName = peer?.let { "Pixel" },
            downloadStatus = status)
    private fun ui(record: TransferMessage) = record.toUi(AppState(name = "This phone"), emptyMap())

    @Test fun directRecordsSayWhereTheyWentAndHowTheyEnded() {
        ui(record(true, "m", MobileDeliveryState.SENT)).let { assertTrue(it.direct); assertEquals("Pixel", it.toName); assertNull(it.ended) }
        // Broadcast / computer messages are not direct.
        ui(record(true, null, MobileDeliveryState.SENT)).let { assertFalse(it.direct); assertNull(it.toName) }
        // The other phone went away, or cancelled its receive.
        ui(record(true, "m", MobileDeliveryState.FAILED, MobileDeliveryFailure.PEER_DISCONNECTED)).let { assertEquals("failed", it.ended); assertNull(it.failure) }
        ui(record(true, "m", MobileDeliveryState.FAILED, MobileDeliveryFailure.CANCELLED)).let { assertEquals("cancelled", it.ended); assertNull(it.failure) }
        // An offline phone target says so.
        assertEquals("对方不在线", ui(record(true, "m", MobileDeliveryState.FAILED, MobileDeliveryFailure.NO_TARGET)).failure)
        // Both ends name the actual receiving phone; failures retain their existing reason.
        ui(record(false, "m", download = "downloading")).let { assertTrue(it.direct); assertEquals("This phone", it.toName); assertTrue(it.receivingNow) }
        // The core's download status tells a failed receive from a cancelled one.
        val failed = MobileDownloadStatus.RECEIVE_FAILED
        ui(record(false, "m", failure = MobileDeliveryFailure.PEER_DISCONNECTED, download = "failed", status = failed)).let { assertEquals("failed", it.ended); assertNull(it.failure) }
        ui(record(false, "m", failure = MobileDeliveryFailure.STORAGE_FULL, download = "failed", status = failed)).let { assertEquals("failed", it.ended); assertEquals("手机存储空间不足", it.failure) }
        ui(record(false, "m", failure = MobileDeliveryFailure.CANCELLED, download = "failed", status = MobileDownloadStatus.CANCELLED)).let { assertEquals("cancelled", it.ended) }
    }

    @Test fun theShareCardNamesThePhone() {
        assertEquals("正在发送给 iPhone…", ShareStatusText.sending(null, "iPhone"))
        assertEquals("正在发送给 iPhone… 2.47 MB/s", ShareStatusText.sending(2_470_000.0, "iPhone"))
        assertEquals("正在发送… 2.47 MB/s", ShareStatusText.sending(2_470_000.0))
        assertEquals("已发送给 iPhone，耗时 6.42 秒", ShareStatusText.completed(6.42, "iPhone"))
        assertEquals("已完成，耗时 6.42 秒", ShareStatusText.completed(6.42))
    }

    @Test fun aReceivedNoticeOpensTheTransferPageAtTheMessage() {
        TransferNotices.createChannels(context)
        val notice = TransferNotices.received(context, "iPad", 3, "白板照片.jpg", 42L)
        assertEquals("iPad", notice.extras.getCharSequence(Notification.EXTRA_TITLE).toString())
        assertEquals("发来 3 个文件：白板照片.jpg 等", notice.extras.getCharSequence(Notification.EXTRA_TEXT).toString())
        assertEquals(TransferNotices.CHANNEL_RECEIVED, notice.channelId)
        val intent: Intent = shadowOf(notice.contentIntent).savedIntent
        assertEquals(MainActivity::class.java.name, intent.component?.className)
        assertEquals(42L, intent.getLongExtra(MainActivity.EXTRA_OPEN_MESSAGE, 0L))
    }

    @Test fun aHiddenShareShowsProgressAndCancel() {
        val cancel = PendingIntent.getService(context, 1, Intent(context, ConnectionService::class.java).setAction(ConnectionService.CANCEL_SHARE).putExtra(ConnectionService.EXTRA_SHARE, 7L),
            PendingIntent.FLAG_IMMUTABLE)
        val notice = TransferNotices.sending(context, "iPhone", "3 张照片", 52, cancel)
        assertEquals("正在发送给 iPhone", notice.extras.getCharSequence(Notification.EXTRA_TITLE).toString())
        assertEquals("3 张照片 · 52%", notice.extras.getCharSequence(Notification.EXTRA_TEXT).toString())
        assertEquals(52, notice.extras.getInt(Notification.EXTRA_PROGRESS))
        assertTrue(notice.flags and Notification.FLAG_ONGOING_EVENT != 0)
        assertEquals("取消", notice.actions.single().title.toString())
        assertEquals(7L, shadowOf(notice.actions.single().actionIntent).savedIntent.getLongExtra(ConnectionService.EXTRA_SHARE, 0L))
        assertEquals(NotificationCompat.CATEGORY_PROGRESS, notice.category)
    }
}
