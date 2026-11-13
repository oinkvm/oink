package com.hachiwii.oink

/** Where one outgoing operation goes, decided from the repository state alone. */
internal sealed interface SendRoute {
    /** Send now. [proxy] is null for content the core broadcasts to every online device. */
    data class Ready(val proxy: String?, val label: String) : SendRoute
    /** The core is not running yet, or the device it needs is still connecting. */
    data object Wait : SendRoute
    /** A definitive state: fail at once with [message]. */
    data class Fail(val message: String) : SendRoute
}

/**
 * [target]: the computer the user chose (null: none). [autoProxy]: the system share entry takes the
 * first online computer itself. [startupFailure]: the connection service could not start its core.
 */
internal fun sendRoute(
    connected: Boolean,
    devices: List<Device>,
    target: String?,
    draft: ShareDraft,
    autoProxy: Boolean = false,
    startupFailure: String? = null,
): SendRoute {
    if (!connected) return startupFailure?.let { SendRoute.Fail("连接服务没能启动：$it") } ?: SendRoute.Wait
    val peers = devices.filter { !it.isSelf }
    // No link state yet (a new core before its first snapshot) counts as still connecting.
    fun pending(device: Device) = !device.online &&
        (device.connection == DeviceConnection.CONNECTING || device.connection == null)
    // A phone picked as the target gets images, files and folders straight (native-rewrite.md
    // 「手机点对点传输」); text is still broadcast below. It never falls back to another device.
    peers.firstOrNull { it.id == target && !it.computer }?.takeIf { draft.items.isNotEmpty() }?.let { phone ->
        return when {
            phone.online -> SendRoute.Ready(phone.id, phone.name)
            pending(phone) -> SendRoute.Wait
            else -> SendRoute.Fail("${phone.name} 不在线")
        }
    }
    if (SharePolicy.canBroadcast(draft)) {
        // DECISIONS 13: text and small images go straight to every online device, phones
        // included; no computer is needed.
        val online = peers.filter { it.online }
        val label = peers.firstOrNull { it.id == target && it.online }?.name ?: online.singleOrNull()?.name ?: "所有在线设备"
        return when {
            online.isNotEmpty() -> SendRoute.Ready(null, label)
            peers.any(::pending) -> SendRoute.Wait
            else -> SendRoute.Fail("没有在线的设备")
        }
    }
    val computers = peers.filter { it.computer }
    val candidates = when {
        target != null -> computers.filter { it.id == target }
        autoProxy -> computers
        else -> return SendRoute.Fail("此内容请选一台电脑")
    }
    candidates.firstOrNull { it.online }?.let { return SendRoute.Ready(it.id, it.name) }
    return if (candidates.any(::pending)) SendRoute.Wait else SendRoute.Fail("没有在线的电脑")
}

/**
 * Without the persistent notification the connection runs only while Oink is in use (decision 38).
 * A picker or camera Oink opened for the composer, and a send still in progress, are use (review H18).
 */
internal fun shouldStopService(
    visibleActivities: Int,
    changingConfigurations: Boolean,
    persistentNotification: Boolean,
    picking: Boolean,
    sending: Boolean,
): Boolean = visibleActivities == 0 && !changingConfigurations && !persistentNotification && !picking && !sending
