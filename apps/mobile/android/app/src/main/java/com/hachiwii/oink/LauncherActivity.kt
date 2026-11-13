package com.hachiwii.oink

import android.app.Activity
import android.content.Intent
import android.os.Bundle

/**
 * Target of the launcher icon aliases ([AppIconManager]). Disabling an alias removes every task
 * whose base intent names it, even with DONT_KILL_APP, so a task started from the icon used to
 * close when the icon changed (theme colour or light/dark). This trampoline runs in its own
 * hidden task and starts [MainActivity] in the app task, whose base intent then names
 * MainActivity itself and survives any alias change.
 */
class LauncherActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        startActivity(Intent(intent).setClass(this, MainActivity::class.java)
            .setFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED))
        finish()
    }
}
