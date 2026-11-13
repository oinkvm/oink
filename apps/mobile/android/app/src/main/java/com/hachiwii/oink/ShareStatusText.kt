package com.hachiwii.oink

import java.math.BigDecimal
import java.math.MathContext
import java.math.RoundingMode
import kotlin.math.floor
import kotlin.math.log10

/** Formats the share card's user-facing status without exposing a target name. */
object ShareStatusText {
    private val threeSignificant = MathContext(3, RoundingMode.HALF_UP)
    private val rateUnits = arrayOf("KB/s", "MB/s", "GB/s")

    /** [to]: the phone a share goes straight to (「正在发送给 X…」); null for a computer. */
    fun sending(rateBytesPerSecond: Double?, to: String? = null): String {
        val head = to?.let { "正在发送给 $it…" } ?: "正在发送…"
        if (rateBytesPerSecond == null || !rateBytesPerSecond.isFinite() || rateBytesPerSecond <= 0.0) {
            return head
        }
        var unit = 0
        var value = rateBytesPerSecond / 1_000.0
        while (unit < rateUnits.lastIndex && rounded(value).toDouble() >= 1_000.0) {
            unit += 1
            value = rateBytesPerSecond / 1_000.0.pow(unit + 1)
        }
        return "$head ${formatThreeSignificant(value)} ${rateUnits[unit]}"
    }

    /**
     * The group's only other device is offline; the share starts by itself once it is online (user
     * 2026-11-11). Chinese breaks after 「Oink」 on purpose (share-anim SPEC §2.1); no final period.
     */
    fun waitingForTarget(): String = com.hachiwii.oink.ui.appText("对方不在线，请在对方设备打开 Oink\n并确保网络连接畅通",
        "The other device is offline. Open Oink on it and make sure it's connected to the network")

    fun completed(elapsedSeconds: Double, to: String? = null): String {
        val elapsed = if (elapsedSeconds.isFinite()) elapsedSeconds.coerceAtLeast(0.0) else 0.0
        return (to?.let { "已发送给 $it" } ?: "已完成") + "，耗时 ${formatThreeSignificant(elapsed)} 秒"
    }

    private fun rounded(value: Double): BigDecimal = BigDecimal.valueOf(value).round(threeSignificant)

    private fun formatThreeSignificant(value: Double): String {
        val rounded = rounded(value)
        if (rounded.signum() == 0) return "0"
        val magnitude = rounded.abs().toDouble()
        val decimals = if (magnitude >= 100.0) 0 else if (magnitude >= 10.0) 1 else if (magnitude >= 1.0) 2 else {
            maxOf(0, 2 - floor(log10(magnitude)).toInt())
        }
        return rounded.setScale(decimals, RoundingMode.HALF_UP).toPlainString()
    }

    private fun Double.pow(exponent: Int): Double {
        var result = 1.0
        repeat(exponent) { result *= 1_000.0 }
        return result
    }
}

/** [WAITING]: the core waits for the share's offline target; nothing is recorded or sent yet. */
enum class SharePhase { IDLE, PREPARING, WAITING, SENDING, SUCCEEDED, FAILED }

/** Progress of the current share or composer send, as the share card shows it. */
data class ShareUiState(
    val phase: SharePhase = SharePhase.IDLE,
    val status: String = "",
    val bytes: Long = 0,
    val totalBytes: Long? = null,
    val rateBytesPerSecond: Double? = null,
    val elapsedSeconds: Double? = null,
    /** The device the user picked; [toPhone]: it is a phone (the share goes straight to it). */
    val to: String? = null,
    val toPhone: Boolean = false,
    /** What is being sent (「3 张照片」), for the background notification. */
    val summary: String = "",
    /** The progress card was hidden: the share goes on in the background, its progress in a notification. */
    val hidden: Boolean = false,
    /** The share's first own record once it is sending (the notification opens the transfer page there). */
    val recordId: Long? = null,
) {
    val progress: Float?
        get() = totalBytes?.takeIf { it > 0L }?.let {
            (bytes.toDouble() / it).toFloat().coerceIn(0f, 1f)
        }
}
