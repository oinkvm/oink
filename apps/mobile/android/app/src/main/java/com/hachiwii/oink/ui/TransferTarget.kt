package com.hachiwii.oink.ui

/*
 * Where the transfer page sends (native-rewrite.md 「输入框和设备选择按组内其它设备变化」 and
 * 「2026-11-11 界面稿评审」, user 2026-11-11): every other device of the group counts, computers and
 * phones, online or not. The core reports it as `TransferTargetsView` (contract mobile-core.md
 * 「手机点对点传输与目标设备」); [TransferTargetsUi] is its presentation.
 */

/**
 * [online]: the group's other devices online now, computers first, each in connection order.
 * [otherDevices]: the group's other devices, online or not. [only]: that device when there is
 * exactly one (online or not). [searching]: this session's first connection round is still running.
 */
data class TransferTargetsUi(
    val online: List<PeerUi>,
    val otherDevices: Int,
    val only: PeerUi? = null,
    val searching: Boolean = false,
    val activeDevice: String? = null,
)

/** The targets of a member list, for design scenes and tests (the app shows the core's TransferTargetsView). */
fun transferTargetsOf(peers: List<PeerUi>, searching: Boolean = false) = TransferTargetsUi(
    online = peers.filter { !it.offline && !it.mobile } + peers.filter { !it.offline && it.mobile },
    otherDevices = peers.size, only = peers.singleOrNull(), searching = searching)

/**
 * The composer for the group's other devices. [picker]: the device picker shows (always, unless the
 * group has exactly one other device). [target]: where attachments go. [disabled]: input and + are
 * off. [noneOnline]: the picker shows the dark grey ban sign. [switchedFrom]: the chosen device went
 * offline and [target] took its place (one light hint).
 */
internal data class ComposerTarget(
    val picker: Boolean,
    val target: PeerUi?,
    val disabled: Boolean,
    val noneOnline: Boolean,
    val placeholder: String,
    val switchedFrom: PeerUi? = null,
)

/** Another online device to send to: computers first, then mobile devices, in connection order. */
internal fun fallbackTarget(online: List<PeerUi>): PeerUi? = online.firstOrNull()

/** [chosen]: the device picked last (id); [peers] names it once it is no longer online. */
internal fun composerTarget(targets: TransferTargetsUi, peers: List<PeerUi>, chosen: String?): ComposerTarget {
    targets.only?.let { only ->
        // One other device: no picker, always that one; offline → nothing can be sent.
        val gone = chosen?.takeIf { id -> !targets.searching && targets.online.none { it.id == id } }?.let { id -> peers.firstOrNull { it.id == id } ?: PeerUi(id, id.take(8), "other") }
        return ComposerTarget(false, only, only.offline, false,
            if (only.offline) appText("${only.name} 不在线", "${only.name} is offline") else appText("发送给 ${only.name}", "Send to ${only.name}"), gone)
    }
    val noneOnline = targets.online.isEmpty()
    val picked = targets.online.firstOrNull { it.id == chosen }
    val target = picked ?: targets.online.firstOrNull { it.id == targets.activeDevice } ?: fallbackTarget(targets.online)
    val gone = chosen?.takeIf { !targets.searching && picked == null }?.let { id -> peers.firstOrNull { it.id == id } ?: PeerUi(id, id.take(8), "other") }
    val placeholder = when {
        noneOnline -> appText("没有其它设备在线", "No other devices online")
        target == null -> appText("输入文字，随手传输…", "Type to send…")
        // Files picked for a phone go only to it; through a computer they reach every device.
        target.mobile -> appText("发送给 ${target.name}", "Send to ${target.name}")
        else -> appText("发送给 ${target.name}，可在所有设备粘贴", "Send to ${target.name}, paste on any device")
    }
    return ComposerTarget(true, target, noneOnline, noneOnline, placeholder, gone)
}

internal fun switchedText(from: String) = appText("$from 已离线，已切回当前活跃", "$from is offline. Switched back to Currently active.")
internal fun offlineSendText(name: String) = appText("$name 已离线，暂时无法发送", "$name is offline. Can’t send right now.")

/** What a share or a received set holds, in a few words: 「3 张照片」, one file's name, or 「3 个文件」. */
internal fun itemsSummary(names: List<String>, images: Int): String = when {
    names.isEmpty() -> appText("文字", "Text")
    images == names.size -> appText("${names.size} 张照片", if (names.size == 1) "1 photo" else "${names.size} photos")
    names.size == 1 -> names[0]
    else -> appText("${names.size} 个文件", "${names.size} files")
}

/** Text of the notification for content received straight from a phone while Oink is in the background. */
internal fun receivedNoticeText(count: Int, firstName: String): String = when (count) {
    1 -> appText("发来 $firstName", "Sent you $firstName")
    else -> appText("发来 $count 个文件：$firstName 等", "Sent you $count files: $firstName and more")
}

/** The ongoing notification of a share sent on in the background (its progress card was hidden). */
internal fun sendingNoticeTitle(to: String) = appText("正在发送给 $to", "Sending to $to")
internal fun sendingNoticeText(summary: String, percent: Int) = "$summary · $percent%"

/** The single-member exception includes offline members. Unknown/large shares and phone-only groups ask. */
internal fun automaticShareTarget(targets: TransferTargetsUi, bytes: Long?): PeerUi? {
    targets.only?.let { return it }
    if (bytes == null || bytes < 0 || bytes >= 300_000_000L) return null
    return targets.online.firstOrNull { it.id == targets.activeDevice && it.computer }
        ?: targets.online.firstOrNull { it.computer }
}
