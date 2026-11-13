package com.hachiwii.oink.control

import android.content.Context
import androidx.core.content.edit

/**
 * The control page's own settings, kept on this phone (the 调节 menu and the remembered keyboards).
 * Only the pointer speed reaches the core (`setSettings`); the rest is page state. 接收音频 and
 * 共享麦克风 in 调节 are not here: they are this phone's device settings (the device page's switches).
 * 屏幕方向 is gone (user 2026-11-12): landscape is the full screen, entered by turning the phone or
 * with 全屏 ([Fullscreen]).
 */
data class ControlPrefs(
    /** The Mac trackpad's tracking-speed step 1–10; macOS's default is 4 (user 2026-11-12). */
    val speed: Int = 4,
    /** 单指划动: "pointer" (default) or "scroll". */
    val swipe: String = "pointer",
    /** 键盘布局: the focus computer's layout the emulated keyboard types for ([ControlLink.typedChar]); "auto" = US for now. */
    val layout: String = "auto",
    /**
     * The keyboard kind remembered per orientation (user 2026-11-12): [keyboard] for portrait (and the
     * side layout), default the system keyboard; [keyboardLand] for landscape (= full screen), default
     * the emulated keyboard.
     */
    val keyboard: KeyboardKind = KeyboardKind.SYSTEM,
    val keyboardLand: KeyboardKind = KeyboardKind.EMULATED,
    /** Landscape emulated keyboard expanded to the whole screen (dragged up); 还原 makes it float again. */
    val expanded: Boolean = false,
    /**
     * The portrait system keyboard's last normal key area height in dp (above the navigation bar), stored
     * only when within [EmulatedKeyboardHeight]'s range of the baseline; 0 = none yet. The emulated
     * keyboard takes it.
     */
    val systemKeys: Float = 0f,
    /**
     * 键盘自动弹出 (docs/control-page.md §9): the focus computer's text fields pop the keyboard up and hide
     * it. Off by default (user 2026-11-12); stored as [KEYBOARD_AUTO_POPUP]. The 调节 switch.
     */
    val keyboardAutoPopup: Boolean = false,
) {
    companion object {
        /** SharedPreferences "control" key of [keyboardAutoPopup]. */
        const val KEYBOARD_AUTO_POPUP = "keyboardAutoPopup"
        val SWIPE = listOf("pointer" to "移动指针", "scroll" to "滚动")
        val LAYOUTS = listOf("auto" to "自动", "us" to "美式英语（US）", "uk" to "英式英语（UK）", "de" to "德语（QWERTZ）",
            "fr" to "法语（AZERTY）", "es" to "西班牙语", "jis" to "日语（JIS）", "ko" to "韩语")
        fun label(list: List<Pair<String, String>>, id: String) = list.firstOrNull { it.first == id }?.second ?: list.first().second

        private fun kind(value: String?, default: KeyboardKind) = when (value) {
            "emulated" -> KeyboardKind.EMULATED
            "system" -> KeyboardKind.SYSTEM
            else -> default
        }
        private fun name(kind: KeyboardKind) = if (kind == KeyboardKind.EMULATED) "emulated" else "system"

        fun load(context: Context): ControlPrefs {
            val p = context.getSharedPreferences("control", Context.MODE_PRIVATE)
            return ControlPrefs(
                speed = p.getInt("speed", 4).coerceIn(1, 10),
                swipe = p.getString("swipe", null).takeIf { v -> SWIPE.any { it.first == v } } ?: "pointer",
                layout = p.getString("layout", null).takeIf { v -> LAYOUTS.any { it.first == v } } ?: "auto",
                keyboard = kind(p.getString("keyboard", null), KeyboardKind.SYSTEM),
                keyboardLand = kind(p.getString("keyboardLand", null), KeyboardKind.EMULATED),
                expanded = p.getBoolean("expanded", false),
                systemKeys = p.getFloat("systemKeys", 0f),
                keyboardAutoPopup = p.getBoolean(KEYBOARD_AUTO_POPUP, false),
            )
        }
        fun save(context: Context, value: ControlPrefs) {
            context.getSharedPreferences("control", Context.MODE_PRIVATE).edit {
                putInt("speed", value.speed); putString("swipe", value.swipe); putString("layout", value.layout)
                putString("keyboard", name(value.keyboard)); putString("keyboardLand", name(value.keyboardLand))
                putBoolean("expanded", value.expanded)
                putFloat("systemKeys", value.systemKeys)
                putBoolean(KEYBOARD_AUTO_POPUP, value.keyboardAutoPopup)
            }
        }
    }
}
