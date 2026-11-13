package com.hachiwii.oink

import com.hachiwii.oink.ui.OpenSourceLicenses
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/**
 * The 开源许可 page reads the generated licenses/mobile-android.json that the build packages as
 * assets: Gradle dependencies (releaseRuntimeClasspath), Rust crates, native code and artwork,
 * each with full texts.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class OpenSourceLicensesTest {
    private val context = RuntimeEnvironment.getApplication()
    private val all get() = OpenSourceLicenses.all(context)
    private fun entry(prefix: String) = all.single { it.id.startsWith(prefix) }

    @Test
    fun listsGradleRuntimeDependenciesWithTheApacheText() {
        for (prefix in listOf("maven:androidx.compose.ui:ui-android@", "maven:androidx.core:core@", "maven:org.jetbrains.kotlin:kotlin-stdlib@",
            "maven:org.jetbrains.kotlinx:kotlinx-coroutines-core-jvm@", "maven:com.google.guava:listenablefuture@")) {
            val item = entry(prefix)
            assertEquals(prefix, "Apache-2.0", item.license)
            assertTrue(prefix, item.texts.any { OpenSourceLicenses.text(context, it).contains("Apache License") })
        }
        // JNA is dual-licensed; Oink takes Apache-2.0 and keeps JNA's own notice.
        val jna = entry("maven:net.java.dev.jna:jna@")
        assertEquals("Apache-2.0", jna.license)
        assertTrue(jna.texts.any { OpenSourceLicenses.text(context, it).contains("Java Native Access") })
    }

    @Test
    fun listsRustNativeAndArtworkEntries() {
        entry("cargo:tokio@")
        entry("cargo:jni@")
        entry("native:Opus@")
        entry("native:webrtc-audio-processing@")
        val robot = entry("asset:android-robot-head@")
        assertTrue(robot.notices.any { it.startsWith("The Android robot is reproduced or modified") })
    }

    @Test
    fun everyEntryHasItsFullTexts() {
        assertTrue(all.size > 100)
        for (item in all) {
            assertTrue(item.id, item.texts.isNotEmpty())
            for (sha in item.texts) assertTrue(item.id, OpenSourceLicenses.text(context, sha).isNotBlank())
        }
    }
}
