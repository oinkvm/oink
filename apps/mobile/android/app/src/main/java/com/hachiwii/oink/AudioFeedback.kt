package com.hachiwii.oink

import uniffi.oink_mobile_ffi.MobileMediaFailure

/** Effective capture, not the switch intent: never claim local silence before capture started. */
internal fun audioFeedback(before: LocalAudio, after: LocalAudio, completedAttempt: Boolean = false): List<String> = when {
    (after.forwardFailure != before.forwardFailure || completedAttempt) && after.forwardFailure != MobileMediaFailure.NONE -> listOf(when (after.forwardFailure) {
        MobileMediaFailure.UNAVAILABLE -> "转发音频失败：系统音频正被其它功能占用（如投屏）"
        MobileMediaFailure.SYSTEM_BLOCKED -> "请解锁手机后再开始转发"
        else -> "转发音频已中断，请重试"
    })
    after.forwardRunning && !before.forwardRunning -> listOf("调节本机音量不影响转发", "转发中本机静音")
    else -> emptyList()
}
