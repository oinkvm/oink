package com.hachiwii.oink.recovery

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.annotation.MainThread
import androidx.core.app.NotificationCompat
import androidx.core.app.RemoteInput
import com.hachiwii.oink.MainActivity
import com.hachiwii.oink.PairingServiceFinder
import com.hachiwii.oink.R
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.launch

/** What the pairing notification shows; null: no notification. */
sealed interface PairingNotice {
    /** The system offers a pairing service: reply with the code. [retry]: the previous code failed. */
    data class Input(val port: Int, val retry: Boolean) : PairingNotice
    data object Pairing : PairingNotice
    data object Paired : PairingNotice
    data class Failed(val message: String) : PairingNotice
}

/**
 * [searching]: the settings page shows 第 3 步, or a code sent from the notification just failed (the
 * user may still be in Settings and ask for a new code there). Results of a code typed in the app are
 * shown by the app itself; the immediate in-progress acknowledgement is shared.
 */
internal fun pairingNoticeFor(searching: Boolean, port: Int?, state: PairingCodeState, submissionPending: Boolean = false): PairingNotice? =
    if (submissionPending) PairingNotice.Pairing else when (state) {
    is PairingCodeState.Pairing -> PairingNotice.Pairing
    is PairingCodeState.Paired -> PairingNotice.Paired.takeIf { state.viaNotice }
    is PairingCodeState.Failed -> when {
        // A pairing service other than the one that failed: the system shows a new code.
        searching && port != null && port != state.port -> PairingNotice.Input(port, retry = true)
        state.viaNotice -> PairingNotice.Failed(state.message)
        else -> null
    }
    PairingCodeState.Idle -> if (searching && port != null) PairingNotice.Input(port, retry = false) else null
}

/** NSD keeps looking for the pairing service while [pairingNoticeFor] may still offer the reply field. */
internal fun pairingSearching(stepActive: Boolean, state: PairingCodeState) =
    stepActive || (state is PairingCodeState.Failed && state.viaNotice)

/**
 * The notification that takes the pairing code while the system's 「使用配对码配对设备」 dialog stays
 * open (see [PairingCode] for why the app's own field cannot be used after leaving Settings). It is a
 * heads-up with a reply field: replying neither pauses Settings nor dismisses its dialog.
 */
object PairingCodeNotice {
    const val CHANNEL = "privilege-pairing"
    internal const val KEY_CODE = "pairing_code"
    private const val ID = 2203
    internal val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val stepActive = MutableStateFlow(false)
    private val submissionPending = MutableStateFlow(false)
    private var job: Job? = null

    /** The settings page shows (true) or left (false) 第 3 步 「输入配对码」. */
    @MainThread fun setStepActive(context: Context, active: Boolean) {
        val app = context.applicationContext
        // Entering the step again (「重新配对」) starts over: the last failure is not shown any more.
        if (active) PairingCode.of(app).reset()
        stepActive.value = active
        if (job == null) job = scope.launch {
            val flow = PairingCode.of(app)
            launch {
                combine(stepActive, flow.state) { step, state -> pairingSearching(step, state) }.distinctUntilChanged()
                    .collect { on -> if (on) PairingServiceFinder.start(app) else PairingServiceFinder.stop() }
            }
            combine(stepActive, flow.state, PairingServiceFinder.port, submissionPending) { step, state, port, pending ->
                pairingNoticeFor(pairingSearching(step, state), port, state, pending)
            }.distinctUntilChanged().collect { show(app, it) }
        }
    }

    /** Acknowledge before flow lookup, locks, NSD, credentials, or pairing work can run. */
    @MainThread internal fun submit(context: Context, code: String, viaNotice: Boolean,
                                   onComplete: () -> Unit = {},
                                   pairing: () -> PairingCodeFlow = { PairingCode.of(context) },
                                   port: () -> Int? = { PairingServiceFinder.port.value }): Job {
        val app = context.applicationContext
        // Updating this same notification synchronously clears RemoteInput's pending spinner.
        // A StateFlow/combine observer is asynchronous and cannot guarantee acknowledgement before
        // pairing work starts; both the notification and split-screen field use this entry point.
        show(app, PairingNotice.Pairing)
        // The code step may disappear before IO starts. Keep its observer from cancelling the
        // acknowledgement during that handoff; the worker publishes the final state before clearing it.
        submissionPending.value = true
        // Leaving the code step stops NSD and clears its port. Snapshot the already-published port
        // before the UI switches to progress; never discover/bind or read credentials here.
        val target = port()
        return scope.launch(Dispatchers.IO) {
            try {
                val flow = pairing()
                if (!flow.submit(code, viaNotice, target) && flow.state.value !is PairingCodeState.Pairing) invalidReply(app)
            } finally { submissionPending.value = false; onComplete() }
        }
    }

    /** A reply that is not six digits: offer the field again (this also ends the reply's spinner). */
    internal fun invalidReply(context: Context) {
        val port = PairingServiceFinder.port.value ?: return show(context, null)
        show(context, PairingNotice.Input(port, retry = false), hint = "配对码是六位数字，请重新输入。")
    }

    private fun show(context: Context, notice: PairingNotice?, hint: String? = null) {
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        if (notice == null) { manager.cancel(ID); return }
        manager.createNotificationChannel(NotificationChannel(CHANNEL, "特权模式配对", NotificationManager.IMPORTANCE_HIGH))
        val builder = NotificationCompat.Builder(context, CHANNEL).setSmallIcon(R.drawable.ic_stat_oink)
        when (notice) {
            is PairingNotice.Input -> {
                val reply = PendingIntent.getBroadcast(context, 0, Intent(context, PairingCodeReceiver::class.java),
                    // RemoteInput fills in the reply, so the PendingIntent must be mutable; the explicit
                    // component keeps it to our own receiver.
                    PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
                val input = RemoteInput.Builder(KEY_CODE).setLabel("六位配对码").build()
                val text = hint ?: if (notice.retry) "刚才的配对码没有成功。请输入「使用配对码配对设备」显示的新配对码，不用离开「设置」。"
                    else "请输入「使用配对码配对设备」显示的六位配对码，不用离开「设置」。"
                // No content intent: opening Oink would leave Settings and close the system's dialog.
                builder.setContentTitle(if (notice.retry) "重新输入配对码" else "已找到配对服务")
                    .setContentText(text).setStyle(NotificationCompat.BigTextStyle().bigText(text))
                    .setCategory(NotificationCompat.CATEGORY_MESSAGE).setPriority(NotificationCompat.PRIORITY_HIGH)
                    .setOnlyAlertOnce(hint != null)
                    .addAction(NotificationCompat.Action.Builder(0, "输入配对码", reply).addRemoteInput(input)
                        .setAllowGeneratedReplies(false).build())
            }
            PairingNotice.Pairing -> builder.setContentTitle("正在配对…").setProgress(0, 0, true).setOngoing(true).setSilent(true)
            PairingNotice.Paired -> builder.setContentTitle("配对成功").setContentText("正在开启特权模式。")
                .setSilent(true).setAutoCancel(true).setTimeoutAfter(10_000).setContentIntent(openOink(context))
            is PairingNotice.Failed -> builder.setContentTitle("配对失败").setContentText(notice.message)
                .setStyle(NotificationCompat.BigTextStyle().bigText(notice.message))
                .setSilent(true).setAutoCancel(true).setContentIntent(openOink(context))
        }
        // Without notification permission this posts nothing; the app's own field still works in split screen.
        runCatching { manager.notify(ID, builder.build()) }
    }

    private fun openOink(context: Context) = PendingIntent.getActivity(context, ID,
        Intent(context, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
}

/** The pairing notification's reply: pairs with the code; the notification then follows the result. */
class PairingCodeReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val app = context.applicationContext
        val code = RemoteInput.getResultsFromIntent(intent)?.getCharSequence(PairingCodeNotice.KEY_CODE)
            ?.toString().orEmpty().filter { !it.isWhitespace() }
        val pending = goAsync()
        PairingCodeNotice.submit(app, code, viaNotice = true, onComplete = { pending.finish() })
    }
}
