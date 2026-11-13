// Android runtime dependency inventory for the license flow (build-support/generate-licenses.mjs).
//
// `:app:oinkLicenseInventory` writes licenses/android-runtime.json from the resolved
// releaseRuntimeClasspath: every Maven module that puts an artifact into the APK, the licenses its
// POM declares (nearest POM in the parent chain, as Maven inherits them) and the legal files
// (LICENSE/NOTICE/COPYING/...) the artifact itself ships, including inside an AAR's jars.
// Nothing here is hand-maintained; the SPDX mapping, policy check and the 开源许可 data are done by
// the node generator from this file.
//
// `checkOinkLicenseInventory` runs before every build (preBuild) and fails when the committed file
// no longer matches the graph, e.g. after adding a dependency. Then run, from the repository root:
//   apps/mobile/android/gradlew -p apps/mobile/android :app:oinkLicenseInventory
//   npm run licenses
// and commit both outputs; the generator rejects unknown or disallowed licenses.
import groovy.json.JsonOutput
import groovy.json.JsonSlurper
import java.security.MessageDigest
import java.util.zip.ZipInputStream
import javax.xml.parsers.DocumentBuilderFactory
import org.gradle.api.artifacts.component.ModuleComponentIdentifier
import org.w3c.dom.Element

val oinkRuntimeInventory = rootProject.file("../../../licenses/android-runtime.json")
val oinkRuntimeConfiguration = "releaseRuntimeClasspath"
val oinkLegalName = Regex("(licen[cs]e|notice|copying|copyright|patents|authors)([._-][\\w.-]*)?", RegexOption.IGNORE_CASE)
val oinkLegalExtensions = setOf("", "txt", "md", "html", "htm")

fun oinkSha256(bytes: ByteArray): String =
    MessageDigest.getInstance("SHA-256").digest(bytes).joinToString("") { "%02x".format(it) }

/** Legal files in a jar/aar, recursing once into nested jars (an AAR's classes.jar and libs/). */
fun oinkLegalFiles(bytes: ByteArray, prefix: String): List<Pair<String, String>> {
    val found = mutableListOf<Pair<String, String>>()
    ZipInputStream(bytes.inputStream()).use { zip ->
        while (true) {
            val entry = zip.nextEntry ?: break
            if (entry.isDirectory) continue
            val data = zip.readBytes()
            val name = entry.name.substringAfterLast('/')
            val extension = if ('.' in name) name.substringAfterLast('.').lowercase() else ""
            if (prefix.isEmpty() && extension == "jar") found += oinkLegalFiles(data, "${entry.name}!/")
            else if (oinkLegalName.matches(name) && (extension in oinkLegalExtensions || name.uppercase().startsWith("LICENSE-"))) {
                found += (prefix + entry.name) to String(data, Charsets.UTF_8)
            }
        }
    }
    return found
}

fun Element.oinkChildren(tag: String): List<Element> =
    (0 until childNodes.length).map { childNodes.item(it) }.filterIsInstance<Element>().filter { it.tagName == tag }

fun Element.oinkText(tag: String): String? = oinkChildren(tag).firstOrNull()?.textContent?.trim()?.takeIf { it.isNotEmpty() }

fun oinkPom(coordinates: String): Element {
    val pom = configurations.detachedConfiguration(dependencies.create("$coordinates@pom")).apply { isTransitive = false }.singleFile
    val factory = DocumentBuilderFactory.newInstance().apply { setFeature("http://apache.org/xml/features/disallow-doctype-decl", true) }
    return factory.newDocumentBuilder().parse(pom).documentElement
}

fun Element.oinkLicenses(): List<Element> = oinkChildren("licenses").flatMap { it.oinkChildren("license") }

fun oinkAndroidRuntimeInventory(): Map<String, Any?> {
    val texts = sortedMapOf<String, String>()
    val artifacts = configurations.getByName(oinkRuntimeConfiguration).incoming.artifacts.artifacts
    val modules = artifacts.groupBy {
        it.id.componentIdentifier as? ModuleComponentIdentifier
            ?: throw GradleException("Non-Maven runtime dependency has no license inventory: ${it.id.displayName}")
    }
    val components = modules.entries.sortedBy { (id, _) -> "${id.group}:${id.module}:${id.version}" }.map { (id, files) ->
        val own = oinkPom("${id.group}:${id.module}:${id.version}")
        var pom = own
        var licensesFrom = "${id.group}:${id.module}:${id.version}"
        while (pom.oinkLicenses().isEmpty()) {
            val parent = pom.oinkChildren("parent").firstOrNull() ?: break
            licensesFrom = listOf("groupId", "artifactId", "version").joinToString(":") {
                parent.oinkText(it) ?: throw GradleException("Incomplete parent POM reference in $licensesFrom")
            }
            pom = oinkPom(licensesFrom)
        }
        // A module can reach the classpath through two variants with the same file (androidx.core:core).
        val shipped = files.distinctBy { it.file }.sortedBy { it.file.name }
        mapOf(
            "group" to id.group, "name" to id.module, "version" to id.version,
            "artifacts" to shipped.map { mapOf("file" to it.file.name, "sha256" to oinkSha256(it.file.readBytes())) },
            "pomLicenses" to pom.oinkLicenses().map { mapOf("name" to it.oinkText("name"), "url" to it.oinkText("url")) },
            "licensesFrom" to licensesFrom,
            "url" to own.oinkText("url"),
            "scm" to own.oinkChildren("scm").firstOrNull()?.oinkText("url"),
            "legalFiles" to shipped.flatMap { artifact ->
                oinkLegalFiles(artifact.file.readBytes(), "").map { (path, text) ->
                    // Hash the decoded text: the generator re-encodes it as UTF-8 to verify it.
                    val sha = oinkSha256(text.toByteArray(Charsets.UTF_8))
                    texts[sha] = text
                    mapOf("artifact" to artifact.file.name, "path" to path, "sha256" to sha)
                }
            }.sortedWith(compareBy({ it["artifact"] }, { it["path"] })),
        )
    }
    return mapOf("schemaVersion" to 1, "configuration" to oinkRuntimeConfiguration, "components" to components, "texts" to texts)
}

fun oinkInventoryJson(): String = JsonOutput.prettyPrint(JsonOutput.toJson(oinkAndroidRuntimeInventory())) + "\n"

tasks.register("oinkLicenseInventory") {
    group = "oink"
    description = "Writes licenses/android-runtime.json from the release runtime dependency graph."
    doLast { oinkRuntimeInventory.writeText(oinkInventoryJson()) }
}

tasks.register("checkOinkLicenseInventory") {
    group = "verification"
    description = "Fails when licenses/android-runtime.json does not match the release runtime dependency graph."
    val marker = layout.buildDirectory.file("oink/license-inventory.checked")
    inputs.files(provider { configurations.getByName(oinkRuntimeConfiguration) })
    inputs.file(oinkRuntimeInventory)
    outputs.file(marker)
    doLast {
        // Compare parsed JSON: a Windows checkout may have converted the committed file to CRLF.
        val committed = if (oinkRuntimeInventory.exists()) JsonSlurper().parse(oinkRuntimeInventory) else null
        if (committed != JsonSlurper().parseText(oinkInventoryJson())) {
            throw GradleException("licenses/android-runtime.json does not match $oinkRuntimeConfiguration. From the repository root run " +
                "`apps/mobile/android/gradlew -p apps/mobile/android :app:oinkLicenseInventory`, then `npm run licenses`, and commit the results.")
        }
        marker.get().asFile.apply { parentFile.mkdirs() }.writeText("ok\n")
    }
}

tasks.matching { it.name == "preBuild" }.configureEach { dependsOn("checkOinkLicenseInventory") }
