package com.hachiwii.oink

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * Finds this phone's own "pair with code" wireless-debugging service (mDNS _adb-tls-pairing._tcp)
 * while the privileged-mode setup shows the code step, so the user only types the six digits.
 * Started and stopped by [com.hachiwii.oink.recovery.PairingCodeNotice], which also offers the code
 * field in a notification once the service is found.
 */
object PairingServiceFinder {
    private val mutablePort = MutableStateFlow<Int?>(null)
    val port = mutablePort.asStateFlow()
    private var manager: NsdManager? = null
    private var listener: NsdManager.DiscoveryListener? = null

    @Synchronized fun start(context: Context) {
        if (listener != null) return
        val nsd = context.getSystemService(NsdManager::class.java) ?: return
        val l = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(type: String) {}
            override fun onDiscoveryStopped(type: String) {}
            override fun onStartDiscoveryFailed(type: String, code: Int) { synchronized(this@PairingServiceFinder) { listener = null } }
            override fun onStopDiscoveryFailed(type: String, code: Int) {}
            override fun onServiceLost(info: NsdServiceInfo) { mutablePort.value = null }
            @Suppress("DEPRECATION")
            override fun onServiceFound(info: NsdServiceInfo) {
                nsd.resolveService(info, object : NsdManager.ResolveListener {
                    override fun onResolveFailed(info: NsdServiceInfo, code: Int) {}
                    override fun onServiceResolved(info: NsdServiceInfo) {
                        // Only this phone's own service: the pairing endpoint must be a local address.
                        val host = info.host ?: return
                        if (com.hachiwii.oink.recovery.LocalAdb.isLocal(host) && info.port in 1..65535) mutablePort.value = info.port
                    }
                })
            }
        }
        runCatching { nsd.discoverServices("_adb-tls-pairing._tcp", NsdManager.PROTOCOL_DNS_SD, l) }.onSuccess { manager = nsd; listener = l }
    }

    @Synchronized fun stop() {
        val l = listener ?: return
        runCatching { manager?.stopServiceDiscovery(l) }
        listener = null; manager = null; mutablePort.value = null
    }
}
