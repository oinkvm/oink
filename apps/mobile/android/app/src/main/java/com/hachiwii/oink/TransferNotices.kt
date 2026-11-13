package com.hachiwii.oink

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import com.hachiwii.oink.ui.appText
import com.hachiwii.oink.ui.receivedNoticeText
import com.hachiwii.oink.ui.sendingNoticeText
import com.hachiwii.oink.ui.sendingNoticeTitle

/**
 * Notifications of phone-to-phone transfers (native-rewrite.md 「手机点对点传输」, AndroidNotice.jsx):
 * content arriving from a phone while Oink is in the background (sender + file name / count; a tap
 * opens the transfer page at it), and a share sent on in the background after its progress card was
 * hidden (progress + 取消).
 */
object TransferNotices {
    const val CHANNEL_RECEIVED = "transfer-received"
    const val CHANNEL_SENDING = "transfer-sending"
    /** Notification ids: hidden shares from here up, by share id; received ones below. */
    private const val SENDING_BASE = 0x10000

    fun createChannels(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(NotificationChannel(CHANNEL_RECEIVED, appText("收到的文件", "Received files"), NotificationManager.IMPORTANCE_HIGH))
        manager.createNotificationChannel(NotificationChannel(CHANNEL_SENDING, appText("发送进度", "Sending progress"), NotificationManager.IMPORTANCE_LOW))
    }

    fun receivedId(recordId: Long) = (recordId % SENDING_BASE).toInt()
    fun sendingId(shareId: Long) = SENDING_BASE + (shareId % SENDING_BASE).toInt()

    /** 「iPhone · 发来 会议纪要.docx」; a tap opens the transfer page at [recordId]. */
    fun received(context: Context, sender: String, count: Int, firstName: String, recordId: Long): Notification {
        val open = Intent(context, MainActivity::class.java).putExtra(MainActivity.EXTRA_OPEN_MESSAGE, recordId)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        val tap = PendingIntent.getActivity(context, receivedId(recordId), open, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        return NotificationCompat.Builder(context, CHANNEL_RECEIVED).setSmallIcon(R.drawable.ic_stat_oink)
            .setContentTitle(sender).setContentText(receivedNoticeText(count, firstName))
            .setCategory(NotificationCompat.CATEGORY_MESSAGE).setPriority(NotificationCompat.PRIORITY_HIGH)
            .setAutoCancel(true).setContentIntent(tap).build()
    }

    /**
     * 「正在发送给 iPhone」「3 张照片 · 52%」 with a progress bar and 取消 ([cancel]). A tap opens the transfer
     * page at the share's own message [recordId] (its newest message before the record is known).
     */
    fun sending(context: Context, to: String, summary: String, percent: Int?, cancel: PendingIntent,
                shareId: Long = 0, recordId: Long? = null): Notification =
        NotificationCompat.Builder(context, CHANNEL_SENDING).setSmallIcon(R.drawable.ic_stat_oink)
            .setContentTitle(sendingNoticeTitle(to))
            .setContentText(if (percent == null) summary else sendingNoticeText(summary, percent))
            .setProgress(100, percent ?: 0, percent == null)
            .setOngoing(true).setOnlyAlertOnce(true).setCategory(NotificationCompat.CATEGORY_PROGRESS)
            .setContentIntent(openTransfer(context, shareId, recordId))
            .addAction(0, appText("取消", "Cancel"), cancel).build()

    /**
     * A hidden share waiting for its offline target (user 2026-11-11): the card's text with 取消 only, no
     * progress. It turns into [sending] once the share starts.
     */
    fun waiting(context: Context, shareId: Long, to: String, cancel: PendingIntent): Notification {
        // One paragraph in a notification: the card's line break is for the card's width.
        val text = ShareStatusText.waitingForTarget().replace("\n", "")
        return NotificationCompat.Builder(context, CHANNEL_SENDING).setSmallIcon(R.drawable.ic_stat_oink)
            .setContentTitle(to).setContentText(text).setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setOngoing(true).setOnlyAlertOnce(true).setCategory(NotificationCompat.CATEGORY_PROGRESS)
            .setContentIntent(openTransfer(context, shareId, null))
            .addAction(0, appText("取消", "Cancel"), cancel).build()
    }

    /** Opens Oink on the transfer page at [recordId] (null: the share has no record yet, its newest message). */
    private fun openTransfer(context: Context, shareId: Long, recordId: Long?): PendingIntent {
        val open = Intent(context, MainActivity::class.java).putExtra(MainActivity.EXTRA_OPEN_MESSAGE, recordId ?: MainActivity.OPEN_TRANSFER_PAGE)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        return PendingIntent.getActivity(context, sendingId(shareId), open, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
    }
}
