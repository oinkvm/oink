package com.hachiwii.oink

import com.hachiwii.oink.recovery.PairingCodeState
import com.hachiwii.oink.recovery.RecoveryPhase

/**
 * 特权模式's state as the settings page shows it ("off", "starting", "on", "failed"), from the
 * recovery status alone. Enabled and waiting for Wi‑Fi / the wireless-debugging service is
 * 正在启动, not 未开启: right after a pairing that is where privileged mode is.
 */
internal fun privilegedFromRecovery(phase: RecoveryPhase, detail: String, enabled: Boolean): String = when (phase) {
    RecoveryPhase.Ready -> "on"
    RecoveryPhase.Connecting, RecoveryPhase.Starting -> if (enabled) "starting" else "off"
    RecoveryPhase.Failed, RecoveryPhase.Expired -> if (enabled) "failed" else "off"
    RecoveryPhase.Pending -> if (enabled && detail == "waiting_for_network") "starting" else "off"
}

/**
 * The page's own override (its "setup" wizard, or a result it shows itself) after 第 3 步's pairing
 * changed; null hands the page back to [privilegedFromRecovery].
 *
 * A pairing finished from the notification happens while Oink is in the background, so the page
 * only sees the final state when it comes back: [PairingCodeState.Paired] must release the "setup"
 * override itself, otherwise the page stays on 设置中 although privileged mode is on (Phone,
 * 2026-11-12).
 */
internal fun privilegedOverrideAfter(pairing: PairingCodeState, current: String?): String? = when (pairing) {
    is PairingCodeState.Pairing -> "starting"
    is PairingCodeState.Failed -> "failed"
    is PairingCodeState.Paired -> null
    PairingCodeState.Idle -> current
}
