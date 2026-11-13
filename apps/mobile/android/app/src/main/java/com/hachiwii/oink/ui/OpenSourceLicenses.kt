package com.hachiwii.oink.ui

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject

/** One entry of the 开源许可 page. */
data class OpenSourceComponent(
    val id: String,
    val name: String,
    val version: String,
    val license: String,
    /** Attribution and modification notices that are not part of the license texts (artwork, vendored code). */
    val notices: List<String>,
    /** SHA-256 names of the full license/notice texts. */
    val texts: List<String>,
)

/**
 * Everything the APK ships, from the generated licenses/mobile-android.json (Rust crates, Gradle
 * dependencies, native code and artwork; build-support/generate-licenses.mjs). The app build copies
 * it and the referenced licenses/texts into the assets (`oinkLicenseAssets` in app/build.gradle.kts).
 */
object OpenSourceLicenses {
    @Volatile private var cached: List<OpenSourceComponent>? = null

    fun all(context: Context): List<OpenSourceComponent> = cached ?: synchronized(this) {
        cached ?: load(context).also { cached = it }
    }

    fun find(context: Context, id: String): OpenSourceComponent? = all(context).firstOrNull { it.id == id }

    fun text(context: Context, sha256: String): String =
        context.assets.open("licenses/texts/$sha256.txt").bufferedReader().use { it.readText() }

    private fun load(context: Context): List<OpenSourceComponent> {
        val index = JSONObject(context.assets.open("licenses/index.json").bufferedReader().use { it.readText() })
        val items = index.getJSONArray("components")
        return (0 until items.length()).map { i ->
            val item = items.getJSONObject(i)
            OpenSourceComponent(item.getString("id"), item.getString("name"), item.getString("version"), item.getString("license"),
                item.getJSONArray("notices").strings(), item.getJSONArray("texts").strings())
        }
    }

    private fun JSONArray.strings(): List<String> = (0 until length()).map { getString(it) }
}
