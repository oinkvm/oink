package com.hachiwii.oink

import android.app.Activity
import android.app.Application
import android.os.Bundle

class OinkApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        com.hachiwii.oink.recovery.RecoveryController.initialize(this)
        // The connection service follows Oink's visible screens (decisions 37 and 38): any visible
        // Oink Activity starts it; with the persistent notification off, the last one leaving stops it.
        registerActivityLifecycleCallbacks(object : ActivityLifecycleCallbacks {
            override fun onActivityStarted(activity: Activity) { repository.activityStarted() }
            override fun onActivityStopped(activity: Activity) { repository.activityStopped(activity.isChangingConfigurations) }
            override fun onActivityCreated(activity: Activity, savedInstanceState: Bundle?) = Unit
            override fun onActivityResumed(activity: Activity) = Unit
            override fun onActivityPaused(activity: Activity) = Unit
            override fun onActivitySaveInstanceState(activity: Activity, outState: Bundle) = Unit
            override fun onActivityDestroyed(activity: Activity) = Unit
        })
    }
    /** The one owner of Oink's Wi-Fi locks: Oink screens and the connection service feed it. */
    internal val wifi: LowLatencyWifi by lazy { LowLatencyWifi(this) }
    // Boot recovery may run before CE is unlocked. Only normal UI/connection entry points
    // evaluate this lazy owner, so helper attachment never opens group identity or content.
    val repository: OinkRepository by lazy {
        check(getSystemService(android.os.UserManager::class.java).isUserUnlocked)
        ShareStaging.clearAbandoned(this)
        android.util.Log.i("Oink", "Rust version: ${uniffi.oink_mobile_ffi.version()}")
        OinkRepository(this).apply { createCore = { RustCoreBridge(this@OinkApplication) }; loadOffline() }
    }
}
