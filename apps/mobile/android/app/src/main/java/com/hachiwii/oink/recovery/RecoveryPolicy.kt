package com.hachiwii.oink.recovery

/** Pure OS boundary policy, shared by the receiver, service and regression tests. */
internal object RecoveryPolicy {
    fun bootAllowed(api: Int, user: Int) = api >= 33 && user == 0
    /**
     * Privileged mode restarts from a system broadcast only while it is on. After a reboot the
     * user's "开机后自动恢复" switch also decides; an App update (MY_PACKAGE_REPLACED) is not a
     * reboot and keeps a running privileged mode running.
     */
    fun restoreOnBroadcast(action: String, enabled: Boolean, autoRecover: Boolean) = enabled && when (action) {
        android.content.Intent.ACTION_LOCKED_BOOT_COMPLETED, android.content.Intent.ACTION_BOOT_COMPLETED -> autoRecover
        android.content.Intent.ACTION_MY_PACKAGE_REPLACED -> true
        else -> false
    }
    fun canWriteSettings(api: Int, permission: Boolean) = api >= 33 && permission
    fun needsPermission(automatic: Boolean, api: Int, permission: Boolean) =
        automatic && api >= 33 && !permission
    /**
     * Android 11-12 (API 30-32) cannot turn wireless debugging on by itself after a reboot, and
     * PC-assisted recovery is not in 0.4.0, so the UI shows
     * "重启后需先在开发者选项中打开无线调试，再回到 Oink 启动".
     */
    fun rebootNeedsWirelessDebugging(api: Int) = api in 30..32
}
