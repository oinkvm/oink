package com.hachiwii.oink

import android.content.Context
import android.database.ContentObserver
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings

/**
 * The phone's own system device name, used as Oink's default device name (decision 38).
 * `Settings.Global.DEVICE_NAME` is the name the user sees in "About phone" (and that
 * Bluetooth/hotspot use); it is readable without permissions. When a ROM leaves it unset,
 * fall back to the marketing-style "Manufacturer Model" pair.
 */
object SystemDeviceName {
    fun read(context: Context): String = choose(
        runCatching { Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME) }.getOrNull(),
        Build.MANUFACTURER, Build.MODEL,
    )

    internal fun choose(global: String?, manufacturer: String?, model: String?): String {
        global?.trim()?.takeIf { it.isNotEmpty() }?.let { return it }
        val maker = manufacturer?.trim().orEmpty()
        val product = model?.trim().orEmpty()
        return when {
            product.isEmpty() && maker.isEmpty() -> "Android"
            product.isEmpty() -> maker.replaceFirstChar { it.titlecase() }
            maker.isEmpty() || product.startsWith(maker, ignoreCase = true) -> product
            else -> "${maker.replaceFirstChar { it.titlecase() }} $product"
        }
    }

    /** Calls [onChange] on the main thread whenever the system device name changes. */
    fun observe(context: Context, onChange: (String) -> Unit): ContentObserver {
        val observer = object : ContentObserver(Handler(Looper.getMainLooper())) {
            override fun onChange(selfChange: Boolean) { onChange(read(context)) }
        }
        context.contentResolver.registerContentObserver(Settings.Global.getUriFor(Settings.Global.DEVICE_NAME), false, observer)
        return observer
    }
}
