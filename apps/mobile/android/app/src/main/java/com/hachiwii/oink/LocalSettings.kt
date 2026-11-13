package com.hachiwii.oink

import android.content.ComponentName
import android.content.Context
import android.content.pm.PackageManager
import androidx.core.content.edit

/** Each group owns exactly one alias; wildcard fallback is deliberately opt-in. */
enum class OpenFileGroup(val alias: String, val defaultEnabled: Boolean, val zh: String, val en: String) {
    Images("OpenImages", true, "图片", "Images"),
    Videos("OpenVideos", true, "视频", "Videos"),
    Audio("OpenAudio", true, "音频", "Audio"),
    Documents("OpenDocuments", true, "文档", "Documents"),
    Archives("OpenArchives", true, "压缩包", "Archives"),
    Packages("OpenPackages", false, "安装包（APK）", "Packages (APK)"),
    Other("OpenOther", false, "其他类型", "Other files"),
}

enum class HomeTab { DEVICES, TRANSFER, CONTROL }

/** Settings 「点按通知」 (user 2026-11-11): copy from the shared clipboard (default), or open Oink at a page. */
enum class NoticeTap { COPY, OPEN }

/** Presentation of the core's audio idle timeout (`MobileAudioIdleTimeout`); the core persists the value. */
enum class AudioTimeout(val minutes: Int, val label: String, val core: uniffi.oink_mobile_ffi.MobileAudioIdleTimeout) {
    ONE(1, "1 分钟", uniffi.oink_mobile_ffi.MobileAudioIdleTimeout.ONE_MINUTE),
    FIVE(5, "5 分钟", uniffi.oink_mobile_ffi.MobileAudioIdleTimeout.FIVE_MINUTES),
    TEN(10, "10 分钟", uniffi.oink_mobile_ffi.MobileAudioIdleTimeout.TEN_MINUTES),
    TWENTY(20, "20 分钟", uniffi.oink_mobile_ffi.MobileAudioIdleTimeout.TWENTY_MINUTES),
    THIRTY(30, "30 分钟", uniffi.oink_mobile_ffi.MobileAudioIdleTimeout.THIRTY_MINUTES),
    SIXTY(60, "60 分钟", uniffi.oink_mobile_ffi.MobileAudioIdleTimeout.SIXTY_MINUTES),
    UNLIMITED(0, "无限制", uniffi.oink_mobile_ffi.MobileAudioIdleTimeout.UNLIMITED);
    companion object {
        fun of(value: uniffi.oink_mobile_ffi.MobileAudioIdleTimeout) = entries.first { it.core == value }
        fun fromMinutes(value: Int) = entries.firstOrNull { it.minutes == value } ?: FIVE
    }
}

class LocalSettings(private val context: Context) {
    private val prefs by lazy { context.getSharedPreferences("local_settings", Context.MODE_PRIVATE) }
    var hideInRecents: Boolean
        get() = prefs.getBoolean("hideInRecents", false)
        set(value) { prefs.edit { putBoolean("hideInRecents", value) }; applyRecents() }
    var openFiles: Boolean
        get() = prefs.getBoolean("openFiles", false)
        set(value) { applyAliases(value); prefs.edit { putBoolean("openFiles", value) } }
    var homeTab: HomeTab
        get() = HomeTab.entries.getOrElse(prefs.getInt("homeTab", HomeTab.DEVICES.ordinal)) { HomeTab.DEVICES }
        set(value) { prefs.edit { putInt("homeTab", value.ordinal) } }
    var autoWriteClipboard: Boolean
        get() = prefs.getBoolean("autoWriteClipboard", false)
        set(value) { prefs.edit { putBoolean("autoWriteClipboard", value) } }
    var autoReadClipboard: Boolean
        get() = prefs.getBoolean("autoReadClipboard", false)
        set(value) { prefs.edit { putBoolean("autoReadClipboard", value) } }
    /** Decision 38. Written only through [OinkRepository.setPersistentNotification], which also moves
     * the connection service in or out of the foreground. Image threshold, audio auto-close and echo
     * cancellation are not stored here: the Rust core owns them. */
    var persistentNotification: Boolean
        get() = prefs.getBoolean("persistentNotification", true)
        internal set(value) { prefs.edit { putBoolean("persistentNotification", value) } }
    /** 「点按通知」; written through [OinkRepository.setNoticeTap], which also updates the notification. */
    var noticeTap: NoticeTap
        get() = NoticeTap.entries.getOrElse(prefs.getInt("noticeTap", NoticeTap.COPY.ordinal)) { NoticeTap.COPY }
        internal set(value) { prefs.edit { putInt("noticeTap", value.ordinal) } }
    /** 「打开页面」 of [NoticeTap.OPEN]; written through [OinkRepository.setNoticeTab]. */
    var noticeTab: HomeTab
        get() = HomeTab.entries.getOrElse(prefs.getInt("noticeTab", HomeTab.DEVICES.ordinal)) { HomeTab.DEVICES }
        internal set(value) { prefs.edit { putInt("noticeTab", value.ordinal) } }
    var speakerTap: SpeakerTap
        get() = SpeakerTap.entries.firstOrNull { it.name == prefs.getString("speakerTap", null) } ?: SpeakerTap.EXIT
        set(value) { prefs.edit { putString("speakerTap", value.name) } }
    var themeMode: String
        get() = prefs.getString("themeMode", "system") ?: "system"
        set(value) { prefs.edit { putString("themeMode", value) } }
    var themeColor: String
        get() = prefs.getString("themeColor", "pink") ?: "pink"
        set(value) { prefs.edit { putString("themeColor", value) } }
    var language: String
        get() = prefs.getString("language", "system") ?: "system"
        set(value) { prefs.edit { putString("language", value) } }
    /** The device the transfer page sends to (computer or phone), chosen last; the default next time. */
    var transferTarget: String?
        get() = prefs.getString("transferTarget", null)
        set(value) { prefs.edit { putString("transferTarget", value) } }


    /** Privileged mode is restored after a reboot (Android 13+) only while this is on. */
    // Device-protected: the boot receiver reads it before the user unlocks.
    private val bootPrefs get() = context.createDeviceProtectedStorageContext().getSharedPreferences("local_settings_boot", Context.MODE_PRIVATE)
    var autoRecover: Boolean
        get() = bootPrefs.getBoolean("autoRecover", true)
        set(value) { bootPrefs.edit { putBoolean("autoRecover", value) } }
    fun selected(group: OpenFileGroup) = prefs.getBoolean(group.alias, group.defaultEnabled)
    fun select(group: OpenFileGroup, selected: Boolean) {
        setAlias(group, openFiles && selected)
        prefs.edit { putBoolean(group.alias, selected) }
    }
    private fun setAlias(group: OpenFileGroup, enabled: Boolean) {
        context.packageManager.setComponentEnabledSetting(
            ComponentName(context, "${context.packageName}.${group.alias}"),
            if (enabled) PackageManager.COMPONENT_ENABLED_STATE_ENABLED else PackageManager.COMPONENT_ENABLED_STATE_DISABLED,
            PackageManager.DONT_KILL_APP)
    }
    private fun applyAliases(enabled: Boolean) { OpenFileGroup.entries.forEach { setAlias(it, enabled && selected(it)) } }
    fun applyRecents() {
        val manager = context.getSystemService(android.app.ActivityManager::class.java)
        manager.appTasks.forEach { task ->
            // The notification-copy task must stay hidden even when the user keeps the main App visible.
            val base = task.taskInfo.baseActivity?.className.orEmpty()
            task.setExcludeFromRecents(base.endsWith(".ClipboardCopyActivity") || hideInRecents)
        }
    }
}

