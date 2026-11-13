package com.hachiwii.oink

import android.os.Looper
import org.junit.After
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowToast

/**
 * The notification's 「从共享剪贴板复制」 tap (user 2026-11-11 / 2026-11-12): copied → the invisible
 * Activity just finishes; nothing to copy → only the toast 「没有可复制的内容」; a failure → a toast with
 * its text. It never opens Oink, and it always removes its own task.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class ClipboardCopyActivityTest {
    @After fun reset() { ClipboardCopyActivity.copy = null }

    private fun run(outcome: CopyOutcome): ClipboardCopyActivity {
        var copies = 0
        ClipboardCopyActivity.copy = { copies++; outcome }
        val activity = Robolectric.buildActivity(ClipboardCopyActivity::class.java).setup().get()
        shadowOf(Looper.getMainLooper()).idle()
        assertEquals(1, copies)
        assertTrue(activity.isFinishing)
        // Never Oink.
        assertNull(shadowOf(activity).nextStartedActivity)
        return activity
    }

    @Test fun copiedFinishesQuietly() {
        run(CopyOutcome.Copied)
        assertNull(ShadowToast.getLatestToast())
    }

    @Test fun nothingToCopyOnlySaysSo() {
        run(CopyOutcome.Nothing)
        assertEquals("没有可复制的内容", ShadowToast.getTextOfLatestToast())
    }

    @Test fun aFailedCopyShowsItsReason() {
        run(CopyOutcome.Failed("连接中断"))
        assertEquals("连接中断", ShadowToast.getTextOfLatestToast())
    }
}
