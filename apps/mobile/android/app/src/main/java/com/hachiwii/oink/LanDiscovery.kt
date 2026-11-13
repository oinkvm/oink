package com.hachiwii.oink

import android.content.Context
import android.net.ConnectivityManager
import java.net.NetworkInterface
import android.net.Network
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import java.net.Inet6Address
import androidx.annotation.RequiresApi

/** One discovery session per network generation. Old callbacks are discarded after close. */
class LanDiscovery(context: Context, private val network: Network?, private val generation: Long,
    private val found: (DiscoveredService) -> Unit, private val lost: (String, Long) -> Unit,
    private val failure: (String) -> Unit) {
    private val connectivity = context.getSystemService(ConnectivityManager::class.java)
    private val nsd = context.getSystemService(NsdManager::class.java)
    private val main = Handler(Looper.getMainLooper())
    private var active = false
    private val queue = ArrayDeque<NsdServiceInfo>()
    private val present = mutableMapOf<String, NsdServiceInfo>()
    private var resolving = false
    private val watches = mutableMapOf<String, NsdManager.ServiceInfoCallback>()
    private val multicast = context.applicationContext.getSystemService(WifiManager::class.java)
        .createMulticastLock("oink-discovery").apply { setReferenceCounted(false) }
    private fun key(info: NsdServiceInfo) = info.serviceName + "." + info.serviceType
    private val listener = object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(type: String) = Unit
        override fun onDiscoveryStopped(type: String) = Unit
        override fun onStartDiscoveryFailed(type: String, code: Int) { failure("局域网发现失败：$code") }
        override fun onStopDiscoveryFailed(type: String, code: Int) { failure("停止局域网发现失败：$code") }
        override fun onServiceFound(info: NsdServiceInfo) { main.post {
            if (active && info.serviceType.contains("_oink._udp")) {
                present[key(info)] = info
                if (Build.VERSION.SDK_INT >= 34) watch(info)
                else { queue.add(info); resolveNext() }
            }
        } }
        override fun onServiceLost(info: NsdServiceInfo) { main.post {
            if (active) {
                present.remove(key(info)); lost(key(info), generation)
                if (Build.VERSION.SDK_INT >= 34) watches.remove(key(info))?.let { runCatching { nsd.unregisterServiceInfoCallback(it) } }
            }
        } }
    }
    fun start() {
        active = true
        multicast.acquire()
        if (Build.VERSION.SDK_INT >= 33) {
            nsd.discoverServices("_oink._udp.", NsdManager.PROTOCOL_DNS_SD, network, { main.post(it) }, listener)
        } else nsd.discoverServices("_oink._udp.", NsdManager.PROTOCOL_DNS_SD, listener)
    }
    fun refresh() { main.post {
        if (active && Build.VERSION.SDK_INT < 34) { queue.addAll(present.values); resolveNext() }
    } }
    @Suppress("DEPRECATION")
    private fun resolveNext() {
        if (!active || resolving || queue.isEmpty()) return
        val info = queue.removeFirst()
        resolving = true
        nsd.resolveService(info, object : NsdManager.ResolveListener {
            override fun onResolveFailed(service: NsdServiceInfo, code: Int) { main.post {
                resolving = false
                if (active) { failure("解析附近设备失败：$code"); resolveNext() }
            } }
            override fun onServiceResolved(service: NsdServiceInfo) { main.post {
                resolving = false
                if (active && key(info) in present) {
                    publish(key(info), service)
                }
                resolveNext()
            } }
        })
    }
    @RequiresApi(34)
    private fun watch(info: NsdServiceInfo) {
        val serviceKey = key(info)
        if (serviceKey in watches) return
        // One-shot resolve omits later addresses and TXT/port changes. Keep the
        // API 34+ subscription until this browser loses the service or closes.
        val callback = object : NsdManager.ServiceInfoCallback {
            override fun onServiceInfoCallbackRegistrationFailed(code: Int) {
                if (watches[serviceKey] === this) watches.remove(serviceKey)
                if (active) failure("订阅附近设备失败：$code")
            }
            override fun onServiceInfoCallbackUnregistered() = Unit
            override fun onServiceLost() {
                if (active && watches[serviceKey] === this) lost(serviceKey, generation)
            }
            override fun onServiceUpdated(service: NsdServiceInfo) {
                if (active && serviceKey in present && watches[serviceKey] === this) publish(serviceKey, service)
            }
        }
        watches[serviceKey] = callback
        nsd.registerServiceInfoCallback(info, { main.post(it) }, callback)
    }
    @Suppress("DEPRECATION")
    private fun publish(serviceKey: String, service: NsdServiceInfo) {
        val addresses = if (Build.VERSION.SDK_INT >= 34) service.hostAddresses else listOfNotNull(service.host)
        // Before API 33 discovery is not scoped to our preferred Network, so borrowing
        // its interface could route a link-local address onto the wrong link.
        val serviceNetwork = if (Build.VERSION.SDK_INT >= 33) service.network ?: network else null
        val interfaceIndex = serviceNetwork?.let { connectivity.getLinkProperties(it)?.interfaceName }
            ?.let { name -> runCatching { NetworkInterface.getByName(name)?.index }.getOrNull() }
            ?.takeIf { it > 0 } ?: 0
        val endpoints = addresses.filter { !it.isLoopbackAddress && !it.isAnyLocalAddress }.map {
            // NSD can carry the selected Network separately from the numeric address.
            val ipv6 = it as? Inet6Address
            val scope = ipv6?.scopeId?.takeIf { id -> id != 0 } ?: if (ipv6?.isLinkLocalAddress == true) interfaceIndex else 0
            Endpoint(it.hostAddress!!.substringBefore('%'), service.port, scope)
        }
        if (endpoints.isEmpty()) lost(serviceKey, generation)
        else found(DiscoveredService(serviceKey, generation, service.attributes.mapValues { it.value.toString(Charsets.UTF_8) }, endpoints))
    }
    fun close() {
        if (!active) return
        active = false
        queue.clear(); present.clear()
        runCatching { nsd.stopServiceDiscovery(listener) }
        if (Build.VERSION.SDK_INT >= 34) {
            watches.values.forEach { callback -> runCatching { nsd.unregisterServiceInfoCallback(callback) } }
            watches.clear()
        }
        if (multicast.isHeld) multicast.release()
    }
}
