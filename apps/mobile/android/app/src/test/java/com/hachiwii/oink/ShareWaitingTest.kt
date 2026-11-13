package com.hachiwii.oink

import android.app.Notification
import android.app.PendingIntent
import android.content.Intent
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import com.hachiwii.oink.ui.Accent
import com.hachiwii.oink.ui.OinkColors
import com.hachiwii.oink.ui.OinkTheme
import com.hachiwii.oink.ui.ShareCardLayer
import com.hachiwii.oink.ui.ShareCardUi
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/**
 * A share to the group's only other device while it is offline (user 2026-11-11): the card and the
 * hidden share's notification say so with 取消 only, then show progress once it starts; the progress
 * notification opens the transfer page at the share's message.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "zh-rCN", application = android.app.Application::class)
class ShareWaitingTest {
    @get:Rule val compose = createComposeRule()
    private val context = RuntimeEnvironment.getApplication()
    /** Broken after 「Oink」 on the card (share-anim SPEC §2.1); one paragraph in the notification. */
    private val waitingText = "对方不在线，请在对方设备打开 Oink\n并确保网络连接畅通"
    private val noticeText = "对方不在线，请在对方设备打开 Oink并确保网络连接畅通"
    private fun cancelIntent(share: Long) = PendingIntent.getService(context, 1,
        Intent(context, ConnectionService::class.java).setAction(ConnectionService.CANCEL_SHARE).putExtra(ConnectionService.EXTRA_SHARE, share),
        PendingIntent.FLAG_IMMUTABLE)

    @Test fun theWaitingTextHasNoTrailingPeriod() {
        assertEquals(waitingText, ShareStatusText.waitingForTarget())
    }

    @Test fun aWaitingShareIsTheWaitingCardThenTheProgressCard() {
        val waiting = ShareUiState(SharePhase.WAITING, ShareStatusText.waitingForTarget(), to = "iPhone", toPhone = true)
        assertEquals(ShareCardUi("waiting", to = "iPhone"), waiting.toCard(true))
        // Started: the usual progress card (no rate sample yet), then real progress.
        val started = waiting.copy(phase = SharePhase.SENDING, status = ShareStatusText.sending(null, "iPhone"))
        assertEquals(ShareCardUi("indeterminate", to = "iPhone"), started.toCard(true))
        assertEquals("sending", started.copy(bytes = 50, totalBytes = 100).toCard(true)?.phase)
        // A computer as the only device: the same waiting card, its stage ends on a computer.
        assertEquals(ShareCardUi("waiting"), waiting.copy(to = "Windows PC", toPhone = false).toCard(true))
    }

    /** No 隐藏 while waiting, not even with a hide handler (share-anim SPEC §2.1). */
    @Test fun theWaitingCardShowsTheTextAndOnlyCancel() {
        var cancelled = 0
        var hidden = 0
        compose.setContent {
            OinkTheme(OinkColors.of(false, Accent.Pink)) {
                ShareCardLayer(ShareCardUi("waiting", to = "iPhone"), host = false, onHide = { hidden++ }, onCancel = { cancelled++ })
            }
        }
        compose.onNodeWithText(waitingText, useUnmergedTree = true).assertIsDisplayed()
        compose.onNodeWithText("隐藏", useUnmergedTree = true).assertDoesNotExist()
        compose.onNodeWithText("取消", useUnmergedTree = true).performClick()
        assertEquals(1, cancelled)
        assertEquals(0, hidden)
    }

    @Test fun theProgressCardStillOffersHideAndCancel() {
        compose.setContent {
            OinkTheme(OinkColors.of(false, Accent.Pink)) {
                ShareCardLayer(ShareCardUi("sending", 40, freezeT = .3f, to = "iPhone"), host = false, onHide = {}, onCancel = {})
            }
        }
        compose.onNodeWithText(waitingText, useUnmergedTree = true).assertDoesNotExist()
        compose.onNodeWithText("隐藏", useUnmergedTree = true).assertIsDisplayed()
        compose.onNodeWithText("取消", useUnmergedTree = true).assertIsDisplayed()
    }

    @Test fun aHiddenWaitingShareNotifiesTheWaitingTextWithCancel() {
        TransferNotices.createChannels(context)
        val notice = TransferNotices.waiting(context, 7L, "iPhone", cancelIntent(7L))
        assertEquals("iPhone", notice.extras.getCharSequence(Notification.EXTRA_TITLE).toString())
        assertEquals(noticeText, notice.extras.getCharSequence(Notification.EXTRA_TEXT).toString())
        // No progress while nothing is sent.
        assertEquals(0, notice.extras.getInt(Notification.EXTRA_PROGRESS_MAX))
        assertTrue(notice.flags and Notification.FLAG_ONGOING_EVENT != 0)
        assertEquals("取消", notice.actions.single().title.toString())
        assertEquals(7L, shadowOf(notice.actions.single().actionIntent).savedIntent.getLongExtra(ConnectionService.EXTRA_SHARE, 0L))
        // A tap opens the transfer page (no record yet: at its newest message).
        val open: Intent = shadowOf(notice.contentIntent).savedIntent
        assertEquals(MainActivity::class.java.name, open.component?.className)
        assertEquals(MainActivity.OPEN_TRANSFER_PAGE, open.getLongExtra(MainActivity.EXTRA_OPEN_MESSAGE, 0L))
    }

    @Test fun tappingTheProgressNotificationOpensTheTransferPageAtTheMessage() {
        TransferNotices.createChannels(context)
        val notice = TransferNotices.sending(context, "iPhone", "3 张照片", 52, cancelIntent(7L), 7L, 42L)
        assertEquals("正在发送给 iPhone", notice.extras.getCharSequence(Notification.EXTRA_TITLE).toString())
        val open: Intent = shadowOf(notice.contentIntent).savedIntent
        assertEquals(MainActivity::class.java.name, open.component?.className)
        assertEquals(42L, open.getLongExtra(MainActivity.EXTRA_OPEN_MESSAGE, 0L))
        assertTrue(open.flags and Intent.FLAG_ACTIVITY_SINGLE_TOP != 0)
        // Before the first progress (record not known yet): the transfer page at its newest message.
        val early: Intent = shadowOf(TransferNotices.sending(context, "iPhone", "3 张照片", null, cancelIntent(7L), 7L, null).contentIntent).savedIntent
        assertEquals(MainActivity.OPEN_TRANSFER_PAGE, early.getLongExtra(MainActivity.EXTRA_OPEN_MESSAGE, 0L))
    }
}
