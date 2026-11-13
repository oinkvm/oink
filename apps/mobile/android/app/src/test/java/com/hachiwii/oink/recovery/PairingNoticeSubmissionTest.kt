package com.hachiwii.oink.recovery

import android.app.Notification
import android.app.NotificationManager
import android.app.NotificationChannel
import android.content.Context
import android.os.Looper
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/** The acknowledgement precedes even flow lookup; no credentials or real pairing are touched. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class PairingNoticeSubmissionTest {
    @Test fun notificationReplyAcknowledgesBeforeAnyLookupOrPairing() = check(viaNotice = true)
    @Test fun splitScreenSubmissionUsesTheSameImmediateAcknowledgement() = check(viaNotice = false)

    private fun check(viaNotice: Boolean) = runBlocking {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(NotificationChannel(PairingCodeNotice.CHANNEL, "Pairing", NotificationManager.IMPORTANCE_HIGH))
        manager.notify(2203, Notification.Builder(context, PairingCodeNotice.CHANNEL).setSmallIcon(com.hachiwii.oink.R.drawable.ic_stat_oink).setContentTitle("输入配对码").build())
        var paired = false
        val flow = PairingCodeFlow({ error("Must use the port captured before the code step is left") }) { _, _ ->
            assertNotEquals(Looper.getMainLooper().thread, Thread.currentThread())
            paired = true
            null
        }
        var port: Int? = 12345
        val job = PairingCodeNotice.submit(context, "123456", viaNotice, port = { port }, pairing = {
            // Even a slow/locked flow lookup must happen after the notification was replaced.
            val notification = shadowOf(manager).allNotifications.single()
            assertEquals("正在配对…", notification.extras.getCharSequence(Notification.EXTRA_TITLE).toString())
            assertTrue(notification.extras.getBoolean(Notification.EXTRA_PROGRESS_INDETERMINATE))
            assertTrue(notification.flags and Notification.FLAG_ONGOING_EVENT != 0)
            assertTrue(notification.actions.isNullOrEmpty())
            assertNull(notification.sound)
            assertEquals(0, notification.defaults and Notification.DEFAULT_SOUND)
            assertNotEquals(Looper.getMainLooper().thread, Thread.currentThread())
            flow
        })
        port = null // NSD clears its published port when the page switches to progress.
        // This is observable before the worker is joined or the main looper runs a collector.
        assertEquals("正在配对…", shadowOf(manager).allNotifications.single().extras.getCharSequence(Notification.EXTRA_TITLE).toString())
        job.join()
        assertTrue(paired)
        assertEquals(PairingCodeState.Paired(viaNotice), flow.state.value)
    }
}
