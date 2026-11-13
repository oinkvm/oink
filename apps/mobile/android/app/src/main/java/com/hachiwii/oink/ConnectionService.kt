package com.hachiwii.oink

import android.app.*
import android.Manifest
import android.content.pm.PackageManager
import androidx.core.content.ContextCompat
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.*
import android.os.*
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.selects.select
import kotlinx.coroutines.sync.withLock

/**
 * Owns the LAN connection (Rust core, NSD, network binding). Started by the app whenever an Oink
 * screen becomes visible (decision 37, no boot start). With the persistent notification ON it runs
 * in the foreground and keeps working after Oink leaves the screen; OFF (decision 38) it never
 * posts a notification and the repository stops it when the last Oink screen goes away.
 *
 * It is Oink's only foreground service with the only persistent notification (user 2026-11-11): the
 * former separate audio service is merged in. Its types are connectedDevice, plus mediaPlayback and
 * microphone while this phone plays or shares audio ([addAudioTypes], [retainAudioTypes]). Because
 * the service starts while an Oink screen is visible and stays in the foreground, it keeps the
 * while-in-use eligibility Android needs for adding the microphone type later from the background
 * (a tile, a computer asking for the microphone).
 */
class ConnectionService : Service() {
    companion object {
        /** 取消 on a hidden share's progress notification; [EXTRA_SHARE] names the share. */
        const val CANCEL_SHARE = "com.hachiwii.oink.CANCEL_SHARE"
        const val EXTRA_SHARE = "com.hachiwii.oink.SHARE"
        /** Set on intents sent with startForegroundService: this start must enter the foreground. */
        const val EXTRA_FOREGROUND = "com.hachiwii.oink.FOREGROUND"
        private const val CHANNEL = PersistentNotices.CHANNEL
        private const val NOTIFICATION = PersistentNotices.ID
        // Main thread only; cleared in onDestroy.
        @android.annotation.SuppressLint("StaticFieldLeak")
        @Volatile private var running: ConnectionService? = null

        /** The service runs in the foreground with the persistent notification. */
        val inForeground: Boolean get() = running?.foreground == true

        /**
         * Adds audio types ([AUDIO_PLAYBACK], [AUDIO_MICROPHONE]) to the persistent foreground service.
         * Main thread. False when the service is not in the persistent foreground: without the persistent
         * notification Oink works only while it is on screen (decision 38), which needs no foreground
         * service. Throws when Android refuses the type (SecurityException, ForegroundServiceStartNotAllowedException).
         */
        @androidx.annotation.MainThread
        fun addAudioTypes(bits: Int): Boolean {
            val service = running ?: return false
            return service.applyAudioTypes(service.audioTypes or bits)
        }

        /** Drops the audio types not in [bits] (audio stopped). Main thread. */
        @androidx.annotation.MainThread
        fun retainAudioTypes(bits: Int) {
            val service = running ?: return
            service.applyAudioTypes(service.audioTypes and bits)
        }
    }
    private val repository get() = (application as OinkApplication).repository
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val connectivity by lazy { getSystemService(ConnectivityManager::class.java) }
    private val networks = linkedSetOf<Network>()
    private val changes = Channel<Unit>(Channel.CONFLATED)
    /** Explicit start requests after a startup failure: 重试, or Oink opened again. */
    private val retries = Channel<Unit>(Channel.CONFLATED)
    /** The core's network layer failed; the value is the core's restart delay in ms (user 2026-11-12). */
    private val networkFailures = Channel<Long>(Channel.CONFLATED)
    /** Ends the running core so the loop below starts a new one after [afterMillis]. */
    private class NetworkRestart(val afterMillis: Long) : Exception("network layer failed")
    private var selected: Network? = null
    private var generation = 0L
    private var discovery: LanDiscovery? = null
    private var advertisement: LanAdvertisement? = null
    private var foreground = false
    /** Audio bits of the current foreground types (see [foregroundServiceTypes]). */
    private var audioTypes = 0
    /** A hidden share keeps the service in the foreground with its progress notification (this id). */
    private var shareForeground: Int? = null
    private var destroyed = false
    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) { scope.launch { networks.add(network); changes.trySend(Unit) } }
        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) { changes.trySend(Unit) }
        override fun onLost(network: Network) { scope.launch { networks.remove(network); changes.trySend(Unit) } }
        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) { scope.launch {
            networks.add(network)
            changes.trySend(Unit)
        } }
    }
    override fun onCreate() {
        super.onCreate()
        running = this
        getSystemService(NotificationManager::class.java).createNotificationChannel(NotificationChannel(CHANNEL, "电脑连接", NotificationManager.IMPORTANCE_LOW))
        TransferNotices.createChannels(this)
        connectivity.registerNetworkCallback(NetworkRequest.Builder()
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_TRUSTED)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN).build(), callback)
        // Seed current networks so startup never depends on a callback. Only networks the callback
        // tracks: others (VPNs) would never receive onLost and stay in the set.
        networks.addAll(connectivity.allNetworks.filter {
            connectivity.getNetworkCapabilities(it)?.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN) == true
        })
        scope.launch {
            // A previous instance may still be retiring its core and network binding after its
            // onDestroy (the app left and came back quickly); attach only after it detached.
            repository.serviceLock.withLock { own() }
        }
    }

    private suspend fun own() {
        repository.serviceEnabled(true)
        repository.status("正在连接…")
        repository.onRefreshNearby = { discovery?.refresh() }
        repository.onDirectIncoming = { sender, count, first, record -> scope.launch { showReceived(sender, count, first, record) } }
        repository.onHiddenShare = { id, share -> scope.launch { showHiddenShare(id, share) } }
        repository.onAdvertisement = { advertisement?.update(it) }
        val notices = scope.launch {
            // Online count, audio roles and the 「点按通知」 setting change it; nothing else (the system
            // also rate-limits updates of one notification).
            repository.state.map { persistentNotice(it) }.distinctUntilChanged().collect { showNotification(it) }
        }
        // Wi-Fi out of power save while this phone is the speaker exit or shares its microphone
        // (user 2026-11-11), also with the screen off; released when the role ends or the service stops.
        val audioWifi = scope.launch { followAudioRole(repository.state, (application as OinkApplication).wifi) }
        repository.onRetryConnection = { retries.trySend(Unit) }
        repository.onNetworkFailed = { networkFailures.trySend(it) }
        try {
            while (true) {
                var previous = networkSnapshot()
                try {
                    selected = preferredNetwork()
                    // bindProcessToNetwork only applies to sockets created afterwards, so it must
                    // precede the core opening its listeners.
                    bindPreferredNetwork()
                    advertisement = LanAdvertisement(this@ConnectionService, selected, repository::status)
                    previous = networkSnapshot()
                    // A failure still queued belongs to a core that is already stopped.
                    networkFailures.tryReceive()
                    withContext(Dispatchers.IO) { repository.startCore() }
                    startDiscovery()
                    while (true) {
                        val restartAfter = select<Long?> {
                            changes.onReceive { null }
                            networkFailures.onReceive { it }
                        }
                        if (restartAfter != null) throw NetworkRestart(restartAfter)
                        val current = networkSnapshot()
                        val next = preferredNetwork()
                        if (current == previous && next == selected) continue
                        previous = current
                        val moved = next != selected
                        selected = next
                        bindPreferredNetwork()
                        discovery?.close(); discovery = null
                        generation++
                        // The QUIC socket keeps the network it was created on; after the process
                        // moved to another Network it must be reopened there (review H17).
                        if (moved) withContext(Dispatchers.IO) { repository.rebindNetwork() }
                        repository.networkChanged(generation)
                        advertisement?.network(selected)
                        startDiscovery()
                    }
                } catch (e: CancellationException) {
                    throw e
                } catch (e: NetworkRestart) {
                    // The core cannot recover its network (its QUIC endpoint stopped): stop it and
                    // start a new one through this same path after the core's back-off (1 s doubling
                    // to 30 s, reset after a stable minute), not on the next network change.
                    android.util.Log.w("ConnectionService", "core network failed; restarting in ${e.afterMillis} ms")
                    closeNativeDiscovery()
                    withContext(Dispatchers.IO) { repository.stopCore() }
                    connectivity.bindProcessToNetwork(null)
                    repository.status("网络出错，正在重新连接…")
                    delay(e.afterMillis)
                    repository.connectionRetrying()
                } catch (e: Exception) {
                    android.util.Log.e("ConnectionService", "connection owner failed", e)
                    closeNativeDiscovery()
                    withContext(Dispatchers.IO) { repository.stopCore() }
                    connectivity.bindProcessToNetwork(null)
                    repository.startFailed(e.oinkMessage ?: e.javaClass.simpleName)
                    // No timer: start again when the network actually changes or the user asks
                    // (重试, or opening Oink again) (review M40).
                    retries.tryReceive()
                    while (true) {
                        val retry = select {
                            retries.onReceive { true }
                            changes.onReceive { false }
                        }
                        if (retry || networkSnapshot() != previous) break
                    }
                    repository.connectionRetrying()
                }
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            android.util.Log.e("ConnectionService", "connection loop stopped", e)
            repository.status("连接离线")
        } finally {
            withContext(NonCancellable) {
                notices.cancel()
                // Joined, so the locks are released before a newer instance (waiting on serviceLock) follows the role.
                audioWifi.cancelAndJoin()
                closeNativeDiscovery()
                repository.onRetryConnection = null
                repository.onNetworkFailed = null
                repository.onAdvertisement = null
                repository.onDirectIncoming = null
                repository.onHiddenShare = null
                repository.onRefreshNearby = null
                withContext(Dispatchers.IO) { repository.stopCore() }
                connectivity.bindProcessToNetwork(null)
                repository.serviceEnabled(false)
                repository.status("连接离线")
                // After onDestroy a newer instance may already own the service component.
                if (!destroyed) {
                    if (foreground) stopForeground(STOP_FOREGROUND_REMOVE)
                    stopSelf()
                }
                foreground = false
                audioTypes = 0
            }
        }
    }
    private fun preferredNetwork(): Network? = selected?.takeIf {
        it in networks && isLan(connectivity.getNetworkCapabilities(it))
    } ?: networks.firstOrNull { isLan(connectivity.getNetworkCapabilities(it)) }

    // Interface and addresses only: whole LinkProperties also change on DNS, route and
    // address-lifetime refreshes, which are not a new network path.
    private fun networkSnapshot() = networks.associateWith { network ->
        connectivity.getLinkProperties(network)?.let { lp -> lp.interfaceName to lp.linkAddresses.map { it.address to it.prefixLength }.toSet() }
    }

    private fun bindPreferredNetwork() {
        if (!connectivity.bindProcessToNetwork(selected)) {
            connectivity.bindProcessToNetwork(null)
            repository.status("网络绑定失败，使用系统默认网络")
        }
        android.util.Log.i("OinkNetwork", "process_binding requested=$selected actual=${connectivity.boundNetworkForProcess}")
    }
    private fun startDiscovery() {
        discovery = LanDiscovery(this, selected, generation,
            repository::discovered, repository::lost, repository::status)
        runCatching { discovery?.start() }.onFailure {
            discovery?.close(); discovery = null
            repository.status(it.message ?: "局域网发现启动失败")
        }
    }
    private fun closeNativeDiscovery() {
        discovery?.close(); discovery = null
        advertisement?.close(); advertisement = null
    }
    private fun isLan(caps: NetworkCapabilities?): Boolean = caps != null &&
        !caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) && !caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN) &&
        (caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) || caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET))

    /** Enters or leaves the foreground to match the "常驻通知" setting. */
    private fun applyForegroundMode(mustEnter: Boolean) {
        val persistent = LocalSettings(this).persistentNotification
        if ((persistent || mustEnter) && !foreground) {
            // startForegroundService requires startForeground even if the setting was turned off
            // in between; it is left again right below.
            startForeground(NOTIFICATION, notification(), foregroundServiceTypes(audioTypes))
            foreground = true
        }
        if (!persistent && foreground) {
            stopForeground(STOP_FOREGROUND_REMOVE)
            foreground = false
            audioTypes = 0
        }
    }
    /**
     * Sets the audio part of the foreground types; false (nothing done) while not in the persistent
     * foreground. startForeground on the running service with the same notification id only changes its
     * types; the eligibility it got when it started on screen stays until it leaves the foreground.
     */
    private fun applyAudioTypes(bits: Int): Boolean {
        if (!foreground) return false
        if (bits == audioTypes) return true
        startForeground(NOTIFICATION, notification(), foregroundServiceTypes(bits))
        audioTypes = bits
        return true
    }
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        applyForegroundMode(intent?.getBooleanExtra(EXTRA_FOREGROUND, false) == true)
        // A start request while the core failed to start is the user coming back to Oink.
        if (intent?.action == null && repository.state.value.startupFailure != null) retries.trySend(Unit)
        if (intent?.action == CANCEL_SHARE) intent.getLongExtra(EXTRA_SHARE, 0L).takeIf { it != 0L }?.let(repository::cancelShare)
        // The persistent notification's 停止接收音频 / 停止共享麦克风 (user 2026-11-12): this phone's own settings off.
        if (intent?.action == PersistentNotices.STOP_FORWARD) repository.setLocalForward(false)
        if (intent?.action == PersistentNotices.STOP_SPEAKER) repository.setLocalSpeaker(false)
        if (intent?.action == PersistentNotices.STOP_MICROPHONE) repository.setLocalMicrophone(false)
        return START_NOT_STICKY
    }
    private fun canNotify() = Build.VERSION.SDK_INT < 33 ||
        ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
    /** Content arriving straight from a phone while Oink is not on screen (on screen the page shows it). */
    private fun showReceived(sender: String, count: Int, first: String, record: Long) {
        if (repository.foreground || !canNotify()) return
        getSystemService(NotificationManager::class.java).notify(TransferNotices.receivedId(record), TransferNotices.received(this, sender, count, first, record))
    }
    /**
     * A share whose progress card was hidden goes on here (native-rewrite.md 「手机点对点传输」): its
     * progress in a notification with 取消, which also keeps the service in the foreground while the
     * persistent notification is off. [share] null: it ended.
     */
    private fun showHiddenShare(id: Long, share: ShareUiState?) {
        val notifications = getSystemService(NotificationManager::class.java)
        val noticeId = TransferNotices.sendingId(id)
        if (share == null) {
            if (shareForeground == noticeId) { stopForeground(STOP_FOREGROUND_REMOVE); shareForeground = null }
            notifications.cancel(noticeId)
            return
        }
        val cancel = PendingIntent.getService(this, noticeId, Intent(this, ConnectionService::class.java).setAction(CANCEL_SHARE).putExtra(EXTRA_SHARE, id),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val notice = if (share.phase == SharePhase.WAITING) TransferNotices.waiting(this, id, share.to.orEmpty(), cancel)
            else TransferNotices.sending(this, share.to.orEmpty(), share.summary, share.progress?.let { (it * 100).toInt() }, cancel, id, share.recordId)
        if (!foreground && shareForeground == null) {
            startForeground(noticeId, notice, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
            shareForeground = noticeId
        } else if (canNotify()) notifications.notify(noticeId, notice)
    }
    private fun showNotification(notice: PersistentNotice) {
        // Without the persistent notification Oink posts nothing (decision 38).
        if (!foreground || !canNotify()) return
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION, PersistentNotices.build(this, notice))
    }
    private fun notification(): Notification = PersistentNotices.build(this, persistentNotice(repository.state.value))
    override fun onBind(intent: Intent?) = null
    override fun onDestroy() {
        destroyed = true
        if (running === this) running = null
        runCatching { connectivity.unregisterNetworkCallback(callback) }
        // Cancelling runs own()'s NonCancellable teardown, which releases the service lock.
        scope.cancel()
        super.onDestroy()
    }
}
