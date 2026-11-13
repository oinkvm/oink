package com.hachiwii.oink

import java.util.Locale
import uniffi.oink_mobile_ffi.ClipboardFailure
import uniffi.oink_mobile_ffi.MobileException
import uniffi.oink_mobile_ffi.TransferFailure

internal val Throwable.oinkMessage: String?
    get() = when {
        this is MobileException.Clipboard && reason == ClipboardFailure.UNKNOWN ->
            if (Locale.getDefault().language == "zh")
                "在线电脑没有可获取的剪贴板内容。请先在电脑上复制内容，再重试。"
            else
                "No clipboard content is available from an online computer. Copy something on the computer, then try again."
        this is MobileException.Transfer -> transferFailureText(reason)
        else -> message
    }

/** Typed transfer failures (contract section 9): an offline, evicted or replaced source keeps its own text. */
internal fun transferFailureText(reason: TransferFailure): String = when (reason) {
    TransferFailure.LINK_LOST -> "连接中断，请重试"
    TransferFailure.NO_TARGET -> "没有在线的电脑"
    TransferFailure.SOURCE_OFFLINE -> "来源设备已离线，上线后再试"
    TransferFailure.EVICTED -> "来源设备已不再保留此内容"
    TransferFailure.NO_SUCH_ITEM -> "来源设备上已没有此内容"
    TransferFailure.SUPERSEDED -> "此内容已被来源设备上的新内容替换"
    TransferFailure.UNSUPPORTED -> "来源设备无法提供此内容"
    TransferFailure.EXPIRED -> "共享内容已过期"
    TransferFailure.NOT_FOUND -> "这条消息已不存在"
    TransferFailure.OWN_MESSAGE -> "本机发送的内容没有可下载的来源"
    TransferFailure.IN_PROGRESS -> "正在下载中"
    TransferFailure.STORAGE_FULL -> "手机存储空间不足"
    TransferFailure.CANCELLED -> "已取消"
    TransferFailure.LOCAL_READ -> "无法读取要发送的内容"
    TransferFailure.STORAGE -> "存储空间不足或无法写入"
    TransferFailure.INCOMPATIBLE -> "对方版本不兼容"
    TransferFailure.PAUSED -> "对方已暂停共享"
    TransferFailure.SEND_DISABLED -> "对方关了同步发送"
}

/** A folder listing that ended without a page: content that is gone for good, or why it could not be read now. */
internal fun folderFailureText(reason: TransferFailure): String = when (reason) {
    TransferFailure.EVICTED, TransferFailure.NO_SUCH_ITEM, TransferFailure.SUPERSEDED, TransferFailure.NOT_FOUND -> "文件夹内容已失效"
    else -> "无法获取文件夹内容：" + transferFailureText(reason)
}

/** Why a received record cannot be downloaded, from its stored availability; null while its source can serve it. */
internal fun availabilityText(availability: String): String? = when (availability.lowercase()) {
    "sourceoffline" -> transferFailureText(TransferFailure.SOURCE_OFFLINE)
    "evicted" -> transferFailureText(TransferFailure.EVICTED)
    "nosuchitem" -> transferFailureText(TransferFailure.NO_SUCH_ITEM)
    "superseded" -> transferFailureText(TransferFailure.SUPERSEDED)
    "unsupported" -> transferFailureText(TransferFailure.UNSUPPORTED)
    else -> null
}
