package com.hachiwii.oink.recovery

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.os.IBinder
import android.provider.Settings
import com.hachiwii.oink.privilege.PrivilegeClient
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.first

/** One bounded attempt per resolved endpoint per network epoch. New network/NSD events retry. */
class RecoveryService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private lateinit var connectivity: ConnectivityManager
    private lateinit var nsd: NsdManager
    private var discovery: NsdManager.DiscoveryListener? = null
    private var canWriteSettings = false
    private var generation = 0L
    private var attempt: Job? = null
    private val tried = mutableSetOf<String>()
    private val resolving = ArrayDeque<NsdServiceInfo>()
    private val candidates = ArrayDeque<NsdServiceInfo>()
    private val withdrawn = mutableSetOf<String>()
    private var resolveBusy = false
    private var watchingNetwork = false
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            scope.launch { restartDiscovery() }
        }
        override fun onLost(network: Network) { scope.launch { restartDiscovery() } }
    }
    override fun onBind(intent: Intent?): IBinder? = null
    override fun onCreate() {
        super.onCreate()
        connectivity = getSystemService(ConnectivityManager::class.java)
        nsd = getSystemService(NsdManager::class.java)
        val notifications = getSystemService(NotificationManager::class.java)
        notifications.createNotificationChannel(NotificationChannel("privilege-recovery", "特权恢复", NotificationManager.IMPORTANCE_LOW))
        val notification = Notification.Builder(this, "privilege-recovery")
            .setSmallIcon(android.R.drawable.stat_sys_upload)
            .setContentTitle("Oink 正在恢复特权连接")
            .setContentText("等待可信无线网络和无线调试服务").setOngoing(true).build()
        startForeground(2202, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        scope.launch {
            RecoveryController.status.first { it.phase == RecoveryPhase.Ready || it.detail == "helper_crash_limit" }
            stopSelf()
        }
    }
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (!RecoveryStore(this).enabled) { stopSelf(); return START_NOT_STICKY }
        if (PrivilegeClient.available()) {
            RecoveryController.helperChanged()
            if (RecoveryController.status.value.phase == RecoveryPhase.Ready) stopSelf()
            return START_NOT_STICKY
        }
        val hasCredential = try { RecoveryStore(this).hasCredential } catch (_: Exception) {
            RecoveryController.report(RecoveryPhase.Failed, RecoveryReason.Unavailable, "credential_store_locked")
            stopSelf(); return START_NOT_STICKY
        }
        if (!hasCredential) {
            RecoveryController.report(RecoveryPhase.Failed, RecoveryReason.Unavailable, "local_key_required")
            stopSelf(); return START_NOT_STICKY
        }
        if (watchingNetwork) { restartDiscovery(); return START_NOT_STICKY }
        RecoveryController.report(RecoveryPhase.Pending, detail = "waiting_for_network")
        try {
            // API30–32 only use the user's already-enabled wireless debugging session.
            if (Build.VERSION.SDK_INT >= 33) {
                canWriteSettings = RecoveryPolicy.canWriteSettings(Build.VERSION.SDK_INT,
                    checkSelfPermission(Manifest.permission.WRITE_SECURE_SETTINGS) == PackageManager.PERMISSION_GRANTED)
                if (RecoveryPolicy.needsPermission(intent?.getBooleanExtra("automatic", false) == true, Build.VERSION.SDK_INT, canWriteSettings)) {
                    RecoveryController.report(RecoveryPhase.Failed, RecoveryReason.PermissionDenied, "write_secure_settings_required")
                    stopSelf(); return START_NOT_STICKY
                }
                if (canWriteSettings) enableWirelessDebugging()
            }
            connectivity.registerNetworkCallback(NetworkRequest.Builder()
                .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
                .removeCapability(NetworkCapabilities.NET_CAPABILITY_TRUSTED)
                .addTransportType(NetworkCapabilities.TRANSPORT_WIFI).build(), networkCallback)
            watchingNetwork = true
            restartDiscovery()
        } catch (_: RuntimeException) {
            RecoveryController.report(RecoveryPhase.Failed, RecoveryReason.PermissionDenied, "wireless_debugging_permission")
            stopSelf()
        }
        return START_NOT_STICKY
    }
    private fun enableWirelessDebugging() {
        if (!RecoveryStore(this).enabled || PrivilegeClient.available()) return
        // Standard Shizuku API33+ path. WSS must be granted by the authorized bootstrap;
        // it cannot grant itself and cannot mark a new WLAN trusted.
        check(Settings.Global.putInt(contentResolver, "adb_wifi_enabled", 1))
        check(Settings.Global.putInt(contentResolver, Settings.Global.ADB_ENABLED, 1))
        check(Settings.Global.putLong(contentResolver, "adb_allowed_connection_time", 0))
    }
    private fun stopDiscovery() {
        discovery?.let { runCatching { nsd.stopServiceDiscovery(it) } }
        discovery = null
        resolving.clear()
        candidates.clear()
        withdrawn.clear()
    }
    private fun restartDiscovery() {
        if (!RecoveryStore(this).enabled) { stopSelf(); return }
        if (PrivilegeClient.available()) {
            if (RecoveryController.status.value.phase == RecoveryPhase.Ready) stopSelf()
            return
        }
        // Loss/replacement invalidates queued NSD endpoints and all in-flight callbacks.
        generation++
        RecoveryController.report(RecoveryPhase.Pending, detail = "waiting_for_network")
        val epoch = generation
        stopDiscovery()
        tried.clear()
        attempt?.cancel()
        attempt = null
        val wifi = connectivity.allNetworks.any {
            connectivity.getNetworkCapabilities(it)?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true
        }
        if (!wifi) return
        try { if (canWriteSettings) enableWirelessDebugging() }
        catch (_: RuntimeException) {
            RecoveryController.report(RecoveryPhase.Failed, RecoveryReason.PermissionDenied, "wireless_debugging_permission")
            return
        }
        val listener = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(type: String) {}
            override fun onDiscoveryStopped(type: String) {}
            override fun onStartDiscoveryFailed(type: String, code: Int) {
                scope.launch { if (epoch == generation) RecoveryController.report(RecoveryPhase.Failed, RecoveryReason.ConnectionFailed, "nsd_unavailable") }
            }
            override fun onStopDiscoveryFailed(type: String, code: Int) {}
            override fun onServiceFound(info: NsdServiceInfo) {
                scope.launch {
                    if (epoch == generation) {
                        withdrawn.remove(info.serviceName)
                        resolving.add(info)
                        resolveNext()
                    }
                }
            }
            override fun onServiceLost(info: NsdServiceInfo) {
                scope.launch {
                    if (epoch == generation) {
                        withdrawn.add(info.serviceName)
                        resolving.removeAll { it.serviceName == info.serviceName }
                        candidates.removeAll { it.serviceName == info.serviceName }
                    }
                }
            }
        }
        discovery = listener
        nsd.discoverServices("_adb-tls-connect._tcp.", NsdManager.PROTOCOL_DNS_SD, listener)
    }
    @Suppress("DEPRECATION")
    private fun resolveNext() {
        if (resolveBusy || resolving.isEmpty()) return
        val epoch = generation
        resolveBusy = true
        nsd.resolveService(resolving.removeFirst(), object : NsdManager.ResolveListener {
            override fun onResolveFailed(info: NsdServiceInfo, code: Int) { scope.launch { resolveBusy = false; resolveNext() } }
            override fun onServiceResolved(info: NsdServiceInfo) {
                scope.launch {
                    resolveBusy = false
                    val host = info.host
                    if (epoch == generation && info.serviceName !in withdrawn && host != null && LocalAdb.isLocal(host)) {
                        candidates.add(info)
                        attemptNext()
                    }
                    resolveNext()
                }
            }
        })
    }
    @Suppress("DEPRECATION")
    private fun attemptNext() {
        if (attempt?.isCompleted == false || PrivilegeClient.available()) return
        // Unauthenticated NSD advertisements must not consume a fixed attempt budget.
        // Deduplicate endpoints, but continue after failures until the authentic service is found.
        val candidate = nextRecoveryCandidate(candidates, tried) { "${it.host.hostAddress}:${it.port}" } ?: return
        launchHelper(candidate, generation)
    }
    private fun launchHelper(info: NsdServiceInfo, epoch: Long) {
        attempt = scope.launch {
            try {
                RecoveryController.report(RecoveryPhase.Connecting, detail = "localhost_adb")
                withContext(Dispatchers.IO) {
                    val credentials = RecoveryStore(this@RecoveryService).credentials()
                    try {
                        ensureActive()
                        if (epoch != generation || !RecoveryStore(this@RecoveryService).enabled) return@withContext
                        RecoveryController.report(RecoveryPhase.Starting, detail = "starting_helper")
                        LocalAdb.startHelper(this@RecoveryService, info.host, info.port, credentials)
                    } finally { credentials.forEach { it.fill(0) } }
                }
                // Dispatch waits only for authenticated attach. Ready additionally needs the unlocked Rust owner.
                withTimeout(10_000) { RecoveryController.status.first { PrivilegeClient.available() } }
            } catch (e: CancellationException) {
                if (e is TimeoutCancellationException && epoch == generation) {
                    RecoveryController.report(RecoveryPhase.Failed, RecoveryReason.StartFailed, "helper_attach_timeout")
                } else throw e
            } catch (error: Exception) {
                if (epoch == generation) {
                    val reason = when ((error as? uniffi.oink_mobile_ffi.MobileException.Command)?.code) {
                        uniffi.oink_mobile_ffi.UserFacingCode.PERMISSION_DENIED -> RecoveryReason.PermissionDenied
                        uniffi.oink_mobile_ffi.UserFacingCode.INVALID_VALUE -> RecoveryReason.Unavailable
                        uniffi.oink_mobile_ffi.UserFacingCode.BACKEND_FAILED -> RecoveryReason.StartFailed
                        else -> RecoveryReason.ConnectionFailed
                    }
                    RecoveryController.report(RecoveryPhase.Failed, reason, "local_adb_failed")
                }
            } finally {
                if (epoch == generation) {
                    attempt = null
                    attemptNext()
                }
            }
        }
    }
    override fun onDestroy() {
        generation++
        stopDiscovery()
        if (watchingNetwork) connectivity.unregisterNetworkCallback(networkCallback)
        scope.cancel()
        super.onDestroy()
    }
}
