package com.hachiwii.oink

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.runtime.*
import androidx.core.content.ContextCompat
import androidx.core.content.edit
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.repeatOnLifecycle
import kotlinx.coroutines.launch
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.withContext

open class MainActivity : ComponentActivity() {
    protected val repository get() = (application as OinkApplication).repository
    private val notificationPermission = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) repository.ensureService()
    }
    /** The local microphone switch asks here first (review H13). */
    private val microphonePermission = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        repository.microphonePermissionResult(granted)
    }
    private var scene by mutableStateOf<DesignScene?>(null)
    /** A phone-to-phone notification was tapped: the transfer page opens at this message. */
    var focusMessage by mutableStateOf<Long?>(null)
        private set
    /** The persistent notification or a tile asked for this page; [OpenTab.seq] makes a repeat count. */
    var openTab by mutableStateOf<OpenTab?>(null)
        private set
    /** One-shot requests (tiles) already run, by [LaunchRequest.id]; kept across recreation. */
    private val handledRequests = ArrayList<String>()
    /** A one-shot request waiting for the window focus (reading the clipboard needs it). */
    private var pendingRequest: LaunchRequest? = null
    private var resumedNow = false

    /** [restored]: the Activity is recreated; its page is restored, only an unhandled one-shot still runs. */
    private fun accept(intent: Intent?, restored: Boolean) {
        val request = LaunchRequest.from(intent) ?: return
        if (!restored) request.tab?.let { openTab = OpenTab(it, (openTab?.seq ?: 0) + 1) }
        if (request.oneShot && (request.id == null || request.id !in handledRequests)) pendingRequest = request
        if (resumedNow && hasWindowFocus()) runPendingRequest()
    }

    /**
     * Runs a tile's request once the window has focus: Android lets only the focused app read the
     * clipboard, and onResume often comes before the focus. Marked handled first, so a recreation
     * cannot send twice.
     */
    private fun runPendingRequest() {
        val request = pendingRequest ?: return
        pendingRequest = null
        request.id?.let(handledRequests::add)
        if (request.sendClipboard) {
            val clip = runCatching { getSystemService(android.content.ClipboardManager::class.java).primaryClip }.getOrNull()
            repository.pasteClipboard(classifyClip(clip) { contentResolver.getType(it) })
        }
        request.speaker?.let { repository.setLocalAudioWhenReady(uniffi.oink_mobile_ffi.MobileMediaKind.SPEAKER, it) }
        request.microphone?.let { repository.setLocalAudioWhenReady(uniffi.oink_mobile_ffi.MobileMediaKind.MICROPHONE, it) }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && resumedNow) runPendingRequest()
    }
    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        outState.putStringArrayList(HANDLED_REQUESTS, handledRequests)
    }

    /** The persistent notification needs the permission; with it off Oink never posts one. */
    private fun requestNotificationPermission(settings: LocalSettings) {
        if (Build.VERSION.SDK_INT < 33 || !settings.persistentNotification ||
            ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED) return
        val preferences = getSharedPreferences("local_settings", MODE_PRIVATE)
        if (preferences.getBoolean("notificationPermissionRequested", false)) return
        preferences.edit { putBoolean("notificationPermissionRequested", true) }
        notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge(SystemBarStyle.auto(Color.TRANSPARENT, Color.TRANSPARENT), SystemBarStyle.auto(Color.TRANSPARENT, Color.TRANSPARENT))
        super.onCreate(savedInstanceState)
        lifecycle.addObserver((application as OinkApplication).wifi.screen)
        // OinkApplication starts the connection service whenever an Oink screen becomes visible.
        scene = designScene(intent)
        focusMessage = intent?.getLongExtra(EXTRA_OPEN_MESSAGE, 0L)?.takeIf { it != 0L }
        savedInstanceState?.getStringArrayList(HANDLED_REQUESTS)?.let(handledRequests::addAll)
        accept(intent, restored = savedInstanceState != null)
        setContent {
            OinkScreen(repository, this is ShareActivity, scene, onAppearanceChanged = { mode, color ->
                AppIconManager.apply(this, mode, color, isSystemDark())
                applyBars(mode)
            })
        }
        applyBars(LocalSettings(this).themeMode)
        lifecycleScope.launch {
            repeatOnLifecycle(Lifecycle.State.STARTED) {
                repository.microphonePermissionRequests.collect { microphonePermission.launch(Manifest.permission.RECORD_AUDIO) }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        designScene(intent)?.let { scene = it }
        intent.getLongExtra(EXTRA_OPEN_MESSAGE, 0L).takeIf { it != 0L }?.let { focusMessage = it }
        accept(intent, restored = false)
    }
    /** A page to show, [seq] counting requests (the same page asked again still navigates). */
    data class OpenTab(val tab: HomeTab, val seq: Int)
    companion object {
        private const val HANDLED_REQUESTS = "oink.handledRequests"
        /** Message id the transfer page opens at (phone-to-phone notifications); a missing message opens it at the newest. */
        const val EXTRA_OPEN_MESSAGE = "com.hachiwii.oink.OPEN_MESSAGE"
        /** [EXTRA_OPEN_MESSAGE] of a share that has no record yet: the transfer page at its newest message. */
        const val OPEN_TRANSFER_PAGE = -1L
    }

    /** Debug builds open a named design scene (see DesignHooks); release builds never do. */
    private fun designScene(intent: Intent?): DesignScene? = intent?.getStringExtra("previewScene")?.let { id ->
        DesignHooks.fixture?.invoke(id)?.let { s ->
            val dark = when (intent.getStringExtra("previewTheme")) { "dark" -> true; "light" -> false; else -> s.dark }
            DesignScene(s.model, s.init, s.prefs, dark).also { applyBars(if (dark == true) "dark" else "light") }
        }
    }

    private fun isSystemDark() = (resources.configuration.uiMode and android.content.res.Configuration.UI_MODE_NIGHT_MASK) == android.content.res.Configuration.UI_MODE_NIGHT_YES

    /** Status and navigation bar icons follow the app's own light/dark choice. */
    private fun applyBars(mode: String) {
        val dark = when (mode) { "dark" -> true; "light" -> false; else -> isSystemDark() }
        val style = if (dark) SystemBarStyle.dark(Color.TRANSPARENT) else SystemBarStyle.light(Color.TRANSPARENT, Color.TRANSPARENT)
        enableEdgeToEdge(style, style)
    }

    override fun onResume() {
        super.onResume()
        resumedNow = true
        if (hasWindowFocus()) runPendingRequest()
        val settings = LocalSettings(this)
        settings.applyRecents()
        AppIconManager.apply(this, settings.themeMode, settings.themeColor, isSystemDark())
        if (scene != null) return
        repository.setForeground(this !is ShareActivity)
        if (this !is ShareActivity) requestNotificationPermission(settings)
    }
    override fun onPause() { resumedNow = false; if (scene == null) repository.setForeground(false); super.onPause() }
}

class ShareActivity : MainActivity() {
    /** The repository operation this card shows (review M39); null while the device picker shows.
     * Restored after a process restart, the repository no longer knows it and the card fails
     * instead of spinning. */
    var shareId by mutableStateOf<Long?>(null)
        private set
    /** What is being shared, in a few words (the picker's 「3 张照片」). */
    var knownBytes by mutableStateOf<Long?>(null)
        private set
    var routingReady by mutableStateOf(false)
        private set
    private var metadataJob: Job? = null
    private fun inspectShare(shared: Intent) {
        metadataJob?.cancel()
        routingReady = false
        knownBytes = null
        metadataJob = lifecycleScope.launch {
            knownBytes = withContext(Dispatchers.IO) { ShareStaging.knownBytes(applicationContext, shared) }
            routingReady = true
        }
    }
    var summary by mutableStateOf("")
        private set
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        summary = ShareStaging.summary(applicationContext, intent)
        inspectShare(intent)
        shareId = savedInstanceState?.getLong(SHARE_ID, 0L)?.takeIf { it != 0L }
    }
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        shareId?.let(repository::forgetShare)
        shareId = null
        summary = ShareStaging.summary(applicationContext, intent)
        inspectShare(intent)
    }
    /** The device picked on the picker, or the group's only other device ([only]: the core waits for it while it is offline). */
    fun pick(target: com.hachiwii.oink.ui.PeerUi, only: Boolean) {
        if (shareId != null) return
        val shared = intent
        shareId = repository.startShare(target.id, target.name, target.mobile, summary, waitForTarget = only) { ShareStaging.fromIntent(applicationContext, shared) }
    }
    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        shareId?.let { outState.putLong(SHARE_ID, it) }
    }
    override fun onDestroy() {
        val finished = isFinishing
        super.onDestroy()
        // Only this Activity's operation; staging is deleted by the operation itself (review M36).
        if (finished) shareId?.let(repository::forgetShare)
    }
    private companion object { const val SHARE_ID = "oink.shareId" }
}
