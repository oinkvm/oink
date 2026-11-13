plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}
// Same rule as the PC packages: `-PoinkBuild=<build>` must encode inhouse_version from
// packaging/identity.json (build = major*10^7 + minor*10^5 + patch*10^3 + n). The rule lives only in
// build-support (identity.decode_build via mobile_version.py); Gradle runs it and fails the build
// when it rejects the build. versionCode is the build, versionName `<inhouse_version>-inhouse.<n>`.
val oinkIdentity = groovy.json.JsonSlurper().parse(rootProject.file("../../../packaging/identity.json")) as Map<*, *>
val oinkInhouseVersion = oinkIdentity["inhouse_version"] as String
val oinkVersion: Map<*, *>? = providers.gradleProperty("oinkBuild").orNull?.let { build ->
    // python3 on Windows is the Store alias; the real interpreter there is python.
    val python = providers.gradleProperty("oinkPython").orNull
        ?: if (System.getProperty("os.name").startsWith("Windows")) "python" else "python3"
    val helper = rootProject.file("../../../build-support/mobile_version.py")
    val result = providers.exec {
        commandLine(python, helper.path, build)
        isIgnoreExitValue = true
    }
    val exit = result.result.get().exitValue
    require(exit == 0) { "oinkBuild=$build: ${result.standardError.asText.get().trim()}" }
    groovy.json.JsonSlurper().parseText(result.standardOutput.asText.get()) as Map<*, *>
}
android {
    namespace = "com.hachiwii.oink"
    compileSdk = 36
    buildToolsVersion = "37.0.0"
    ndkVersion = "30.0.16248370"
    defaultConfig {
        applicationId = "com.hachiwii.oink"
        minSdk = 30
        targetSdk = 36
        versionCode = (oinkVersion?.get("build") as Int?) ?: 1
        versionName = (oinkVersion?.get("version_name") as String?) ?: "$oinkInhouseVersion-dev"
        ndk { abiFilters += providers.gradleProperty("oinkAbis").orElse("arm64-v8a").get().split(",") }
    }
    externalNativeBuild { ndkBuild { path = file("src/main/cpp/Android.mk") } }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    buildFeatures { compose = true }
    // ScreenshotTest renders the screens under Robolectric and records PNGs (no emulator needed).
    testOptions { unitTests {
        isIncludeAndroidResources = true
        all { it.systemProperty("roborazzi.test.record", "true") }
    } }
    sourceSets["main"].java.srcDir(rootProject.layout.buildDirectory.dir("generated/oink"))
}
// Gradle runtime dependencies → licenses/android-runtime.json (checked before every build).
apply(from = "licenses.gradle.kts")

/**
 * The 开源许可 page's data: the generated licenses/mobile-android.json (everything the APK ships:
 * Rust crates, Maven dependencies, native code and artwork) plus the full texts it references.
 */
abstract class OinkLicenseAssets : DefaultTask() {
    @get:InputFile @get:PathSensitive(PathSensitivity.NONE) abstract val index: RegularFileProperty
    @get:InputDirectory @get:PathSensitive(PathSensitivity.RELATIVE) abstract val texts: DirectoryProperty
    @get:OutputDirectory abstract val output: DirectoryProperty

    @TaskAction
    fun copy() {
        val out = output.get().asFile.apply { deleteRecursively() }
        val source = index.get().asFile
        source.copyTo(File(out, "licenses/index.json"))
        val components = (groovy.json.JsonSlurper().parse(source) as Map<*, *>)["components"] as List<*>
        components.flatMap { (it as Map<*, *>)["texts"] as List<*> }.map { it as String }.toSortedSet().forEach { sha ->
            File(texts.get().asFile, "$sha.txt").copyTo(File(out, "licenses/texts/$sha.txt"))
        }
    }
}
androidComponents {
    // One task per variant: AGP gives each generated directory its own variant-specific output.
    onVariants { variant ->
        val licenseAssets = tasks.register<OinkLicenseAssets>("oinkLicenseAssets${variant.name.replaceFirstChar { it.uppercase() }}") {
            index.set(rootProject.file("../../../licenses/mobile-android.json"))
            texts.set(rootProject.file("../../../licenses/texts"))
        }
        variant.sources.assets?.addGeneratedSourceDirectory(licenseAssets, OinkLicenseAssets::output)
    }
}
dependencies {
    implementation(platform("androidx.compose:compose-bom:2025.05.01"))
    implementation("androidx.activity:activity-compose:1.10.1")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.9.0")
    implementation("androidx.core:core-ktx:1.16.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    implementation("net.java.dev.jna:jna:5.17.0@aar")
    // The transfer page's media viewer plays videos in the app (ExoPlayer only; its controls are our own).
    implementation("androidx.media3:media3-exoplayer:1.8.0")
    testImplementation("junit:junit:4.13.2")
    testImplementation(platform("androidx.compose:compose-bom:2025.05.01"))
    testImplementation("androidx.compose.ui:ui-test-junit4")
    testImplementation("org.robolectric:robolectric:4.14.1")
    testImplementation("io.github.takahirom.roborazzi:roborazzi:1.43.0")
    testImplementation("io.github.takahirom.roborazzi:roborazzi-compose:1.43.0")
    debugImplementation("androidx.compose.ui:ui-test-manifest")
}
