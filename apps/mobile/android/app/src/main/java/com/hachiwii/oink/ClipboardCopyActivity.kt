package com.hachiwii.oink

import android.content.Context
import android.os.Build
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.launch

/**
 * The persistent notification's tap in 「从共享剪贴板复制」 mode (user 2026-11-11): copies the group's
 * current shared clipboard (text or image) to this phone and finishes at once. With nothing to copy it
 * only shows the toast 「没有可复制的内容」, and when copying fails a toast with the failure's text; it
 * never opens Oink (user 2026-11-12: it used to continue into Oink's main page then).
 *
 * The toast uses the application context (this Activity is gone a moment later; a text toast is drawn
 * by the system, so it stays up after the task is removed) and is shown while this Activity is the
 * resumed foreground app, so Android 11+'s limits on background toasts do not apply. MIUI may prefix
 * the text with the app name.
 *
 * It is an invisible Activity: translucent theme without background, preview or
 * window animations, its own excluded-from-recents task, nothing drawn. The copy runs in the repository
 * (it outlives this Activity); writing the clipboard needs no focus.
 */
class ClipboardCopyActivity : ComponentActivity() {
    companion object {
        /** Tests replace the copy; null: the repository's [OinkRepository.copySharedClipboard]. */
        @androidx.annotation.VisibleForTesting
        internal var copy: (suspend (Context) -> CopyOutcome)? = null
        const val NOTHING_TO_COPY = "没有可复制的内容"
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        noAnimation(open = true)
        lifecycleScope.launch {
            // A copy still running (a second tap, or this Activity recreated) is joined, not repeated.
            val outcome = copy?.invoke(applicationContext)
                ?: (application as OinkApplication).repository.copySharedClipboard().await()
            // Nothing to copy, or it failed: say so; Oink stays closed (user 2026-11-12).
            when (outcome) {
                CopyOutcome.Copied -> {}
                CopyOutcome.Nothing -> toast(NOTHING_TO_COPY)
                is CopyOutcome.Failed -> toast(outcome.message)
            }
            // Not finish(): this Activity finishes only after the copy, so it was resumed and its
            // (empty, black) window snapshotted; MIUI then kept its own task in recents as a black
            // Oink card despite excludeFromRecents. Removing the task drops that card.
            finishAndRemoveTask()
            noAnimation(open = false)
        }
    }

    private fun toast(text: String) = Toast.makeText(applicationContext, text, Toast.LENGTH_SHORT).show()

    @Suppress("DEPRECATION")
    private fun noAnimation(open: Boolean) {
        if (Build.VERSION.SDK_INT >= 34) {
            overrideActivityTransition(if (open) OVERRIDE_TRANSITION_OPEN else OVERRIDE_TRANSITION_CLOSE, 0, 0)
        } else overridePendingTransition(0, 0)
    }
}
