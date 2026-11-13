package com.hachiwii.oink

import android.content.Context

/** The installed version as Settings → About shows it: "<versionName> (<versionCode>)". Both come
 * from packaging/identity.json plus the shared in-house build number (see app/build.gradle.kts). */
object AppVersion {
    fun label(context: Context): String {
        val info = context.packageManager.getPackageInfo(context.packageName, 0)
        return "${info.versionName} (${info.longVersionCode})"
    }
}
