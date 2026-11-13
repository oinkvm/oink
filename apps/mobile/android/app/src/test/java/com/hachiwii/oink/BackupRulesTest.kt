package com.hachiwii.oink

import java.io.File
import javax.xml.parsers.DocumentBuilderFactory
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.w3c.dom.Element

/**
 * Backup and device-transfer rules: only transfer history and settings leave
 * this phone. The core keeps device.key, the group and pairing files and the
 * custom device name in `core/device-only/`
 * (crates/oink-mobile-ffi/src/device_only.rs); those never do.
 */
class BackupRulesTest {
    private data class Rule(val tag: String, val domain: String, val path: String)

    private fun rules(file: String, section: String): List<Rule> {
        val root = DocumentBuilderFactory.newInstance().newDocumentBuilder()
            .parse(File("src/main/res/xml/$file")).documentElement
        val parent = if (root.tagName == section) root else root.getElementsByTagName(section).item(0) as Element
        val children = parent.childNodes
        return (0 until children.length).mapNotNull { children.item(it) as? Element }
            .map { Rule(it.tagName, it.getAttribute("domain"), it.getAttribute("path")) }
    }

    private val cloud get() = rules("data_extraction_rules.xml", "cloud-backup")
    private val transfer get() = rules("data_extraction_rules.xml", "device-transfer")
    /** Android 11 (`fullBackupContent`), the same as cloud-backup. */
    private val legacy get() = rules("backup_rules.xml", "full-backup-content")

    /** With no include rule everything is included except what an exclude covers. */
    private fun excluded(rules: List<Rule>, domain: String, path: String): Boolean {
        assertTrue(rules.none { it.tag == "include" })
        return rules.any {
            it.tag == "exclude" && it.domain == domain &&
                (it.path == "." || it.path == path || (it.path.endsWith("/") && path.startsWith(it.path)))
        }
    }

    private val deviceOnly = listOf(
        "file" to "core/device-only/device.key",
        "file" to "core/device-only/group-state.v2",
        "file" to "core/device-only/device-name.json",
        "file" to "core/mobile-session.lock",
        "sharedpref" to "audio-permissions.xml",
        "device_sharedpref" to "privilege_recovery.xml",
    )
    private val historyAndSettings = listOf(
        "file" to "core/transfer.sqlite",
        "file" to "core/transfer.sqlite-wal",
        "file" to "core/audio-options-v2.json",
        "file" to "core/microphone-controls-v2.cbor",
        "sharedpref" to "local_settings.xml",
        "sharedpref" to "oink.xml",
        "sharedpref" to "control.xml",
        "device_sharedpref" to "local_settings_boot.xml",
    )
    private val attachments = listOf("file" to "core/transfers/a/b.jpg", "file" to "received/c.txt")

    @Test
    fun backupIsOnWithTheRuleFiles() {
        val manifest = File("src/main/AndroidManifest.xml").readText()
        assertTrue(manifest.contains("android:allowBackup=\"true\""))
        assertTrue(manifest.contains("android:fullBackupContent=\"@xml/backup_rules\""))
        assertTrue(manifest.contains("android:dataExtractionRules=\"@xml/data_extraction_rules\""))
    }

    @Test
    fun deviceOnlyFilesNeverLeaveThePhone() {
        for ((name, rules) in listOf("cloud" to cloud, "transfer" to transfer, "legacy" to legacy)) {
            for ((domain, path) in deviceOnly) assertTrue("$name $path", excluded(rules, domain, path))
        }
    }

    @Test
    fun historyAndSettingsAreBackedUpAndTransferred() {
        for ((name, rules) in listOf("cloud" to cloud, "transfer" to transfer, "legacy" to legacy)) {
            for ((domain, path) in historyAndSettings) assertFalse("$name $path", excluded(rules, domain, path))
        }
    }

    /** Cloud backup has a 25 MB quota: attachments only move by device transfer. */
    @Test
    fun attachmentsMoveOnlyByDeviceTransfer() {
        for ((domain, path) in attachments) {
            assertTrue(path, excluded(cloud, domain, path))
            assertTrue(path, excluded(legacy, domain, path))
            assertFalse(path, excluded(transfer, domain, path))
        }
    }
}
