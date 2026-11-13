package com.hachiwii.oink

import android.content.ComponentName
import android.content.Context
import android.content.pm.PackageManager

/**
 * Launcher alias switching for the 5 accent × light/dark adaptive icons.
 * Accent ids are the design-token names (theme.css / UI `Accent.id`): pink, orange, violet, blue,
 * teal — the same names as the launcher aliases and icon resources (`Launcher_violet_Light`, ...).
 */
internal object AppIconManager {
    internal val accents = listOf("pink", "orange", "violet", "blue", "teal")
    /** Unknown ids fall back to the default pink icon. */
    internal fun accentOf(themeColor: String) = if (themeColor in accents) themeColor else "pink"
    private val modes = listOf("light", "dark")

    fun apply(context: Context, themeMode: String, themeColor: String, systemDark: Boolean) {
        val dark = themeMode == "dark" || (themeMode == "system" && systemDark)
        val accent = accentOf(themeColor)
        val selected = "${accent}_${if (dark) "dark" else "light"}"
        val packageManager = context.packageManager
        val aliases = accents.flatMap { color ->
            modes.map { mode ->
                "${context.packageName}.Launcher_${color}_${mode.replaceFirstChar(Char::uppercase)}"
            }
        }
        val selectedAlias = "${context.packageName}.Launcher_${accent}_${if (dark) "Dark" else "Light"}"
        // Enable the destination before disabling the old alias. Some launchers
        // remove the home shortcut if the package briefly has no enabled
        // MAIN/LAUNCHER component during a theme or accent change.
        packageManager.setComponentEnabledSetting(
            ComponentName(context, selectedAlias),
            PackageManager.COMPONENT_ENABLED_STATE_ENABLED,
            PackageManager.DONT_KILL_APP,
        )
        aliases.filter { it != selectedAlias }.forEach { alias ->
            packageManager.setComponentEnabledSetting(
                ComponentName(context, alias),
                PackageManager.COMPONENT_ENABLED_STATE_DISABLED,
                PackageManager.DONT_KILL_APP,
            )
        }
        // Keep the selected name in one place for diagnostics and future shortcut code.
        context.getSharedPreferences("local_settings", Context.MODE_PRIVATE).edit().putString("activeIcon", selected).apply()
    }
}
