package com.hachiwii.oink.recovery

import android.content.Context
import com.hachiwii.oink.PairingServiceFinder
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import uniffi.oink_mobile_ffi.MobileException
import uniffi.oink_mobile_ffi.UserFacingCode

/**
 * 特权模式第 3 步: pairing with this phone's own wireless-debugging service using the six-digit code
 * the system shows in 「使用配对码配对设备」.
 *
 * Why the code also comes from a notification (Android behaviour, not documented as such): the system
 * pairing dialog only lives while Settings is in the foreground. WirelessDebuggingFragment.onPause()
 * removes it and its dismiss calls AdbManager.disablePairing(), which stops the pairing server and its
 * _adb-tls-pairing advertisement. Switching to Oink to type the code therefore always kills the server
 * first (Phone / Android 15, 2026-11-12: "WirelessDebuggingFrag: onDismiss" and "MdnsAdvertiser
 * Removing service" ~0.2 s after Oink came to the front, both attempts, before any code was typed).
 * The code has to be entered while Settings stays resumed: in Oink's notification ([PairingCodeNotice],
 * a heads-up or the shade does not pause Settings), or in the app in split screen. Shizuku does the same.
 *
 * The in-app field and the notification share one [PairingCodeFlow], so the settings page follows a
 * pairing started from the notification and the other way round.
 */
object PairingCode {
    @Volatile private var flow: PairingCodeFlow? = null

    fun of(context: Context): PairingCodeFlow = flow ?: synchronized(this) {
        flow ?: run {
            val app = context.applicationContext
            PairingCodeFlow({ PairingServiceFinder.port.value }) { port, code -> pairLocal(app, port, code) }.also { flow = it }
        }
    }

    private suspend fun pairLocal(context: Context, port: Int, code: String): PairingFailure? = try {
        RecoveryController.pair(context, "127.0.0.1", port, code)
        null
    } catch (e: CancellationException) {
        throw e
    } catch (e: MobileException.Command) {
        when (e.code) {
            // oink-adb: nothing listening on the port (the dialog was closed, so the server is gone).
            UserFacingCode.LINK_LOST -> PairingFailure.NO_SERVICE
            // oink-adb: SPAKE2 / the peer-info exchange failed, i.e. the code does not match.
            UserFacingCode.PERMISSION_DENIED -> PairingFailure.CODE_REJECTED
            else -> PairingFailure.FAILED
        }
    } catch (e: Exception) {
        PairingFailure.FAILED
    }
}

enum class PairingFailure { NO_SERVICE, CODE_REJECTED, FAILED }

internal fun pairingFailureText(failure: PairingFailure): String = when (failure) {
    PairingFailure.NO_SERVICE -> "系统的配对码已失效：离开「设置」时系统会关闭配对码对话框。请在「无线调试」中重新点按「使用配对码配对设备」，不要离开「设置」，在 Oink 的通知里输入新的配对码。"
    PairingFailure.CODE_REJECTED -> "配对码不正确或已过期。请在「无线调试」中重新点按「使用配对码配对设备」，在 Oink 的通知里输入新的配对码。"
    PairingFailure.FAILED -> "无法完成配对，请重试。"
}

sealed interface PairingCodeState {
    /** No attempt, or the user went back to the code step after the last one. */
    data object Idle : PairingCodeState
    data class Pairing(val viaNotice: Boolean) : PairingCodeState
    /** [port] is the pairing service the failed attempt used; a new one means a new code is offered. */
    data class Failed(val failure: PairingFailure, val port: Int?, val viaNotice: Boolean) : PairingCodeState {
        val message: String get() = pairingFailureText(failure)
    }
    data class Paired(val viaNotice: Boolean) : PairingCodeState
}

/**
 * One pairing at a time. [port] is the pairing service found by NSD (null while none is advertised);
 * [pair] returns null on success or why it failed. A success also turns privileged mode on
 * ([RecoveryController.pair]).
 */
class PairingCodeFlow(private val port: () -> Int?, private val pair: suspend (port: Int, code: String) -> PairingFailure?) {
    private val mutableState = MutableStateFlow<PairingCodeState>(PairingCodeState.Idle)
    val state: StateFlow<PairingCodeState> = mutableState.asStateFlow()

    /**
     * Pairs with [code]. Returns false without doing anything while another attempt is running or
     * when [code] is not six digits. [target] may be captured at submission before the UI leaves
     * the setup step and stops NSD; the pairing work still runs off the main thread.
     */
    suspend fun submit(code: String, viaNotice: Boolean, target: Int? = port()): Boolean {
        if (!code.matches(SIX_DIGITS)) return false
        while (true) {
            val current = mutableState.value
            if (current is PairingCodeState.Pairing) return false
            if (mutableState.compareAndSet(current, PairingCodeState.Pairing(viaNotice))) break
        }
        val failure = try {
            if (target == null) PairingFailure.NO_SERVICE else pair(target, code)
        } catch (e: CancellationException) {
            mutableState.value = PairingCodeState.Idle
            throw e
        }
        mutableState.value = if (failure == null) PairingCodeState.Paired(viaNotice) else PairingCodeState.Failed(failure, target, viaNotice)
        return true
    }

    /** Back on the code step (「重新配对」, 「开始设置」): the last result is forgotten, a new code can be entered. */
    fun reset() {
        mutableState.update { if (it is PairingCodeState.Pairing) it else PairingCodeState.Idle }
    }

    private companion object { val SIX_DIGITS = Regex("[0-9]{6}") }
}
