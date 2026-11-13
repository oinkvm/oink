package com.hachiwii.oink.recovery

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build

/** A short broadcast transaction. Network waiting belongs to connectedDevice FGS. */
class RecoveryBootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (!RecoveryPolicy.bootAllowed(Build.VERSION.SDK_INT, android.os.Process.myUid() / 100000)) return
        val action = intent.action ?: return
        if (action !in setOf(Intent.ACTION_LOCKED_BOOT_COMPLETED, Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED)) return
        // Settings "开机后自动恢复" (device-protected, readable before unlock; review M41).
        if (!RecoveryPolicy.restoreOnBroadcast(action, RecoveryStore(context).enabled,
                com.hachiwii.oink.LocalSettings(context).autoRecover)) return
        // ConnectionService is Activity-owned; boot never starts it.
        RecoveryController.start(context, automatic = true)
    }
}
