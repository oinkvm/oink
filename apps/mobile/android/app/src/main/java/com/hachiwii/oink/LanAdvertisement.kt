package com.hachiwii.oink

import android.content.Context
import android.net.Network
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.os.Handler
import android.os.Looper

/** Coalesces owner and network updates; unregister completion is the barrier before replacement. */
class LanAdvertisement(context: Context, private var network: Network?, private val failure: (String) -> Unit) {
    private val nsd = context.getSystemService(NsdManager::class.java)
    private val main = Handler(Looper.getMainLooper())
    private var desired: Advertisement? = null
    private var current: Advertisement? = null
    private var registeredNetwork: Network? = null
    private var listener: NsdManager.RegistrationListener? = null
    private var registering = false
    private var unregistering = false
    private var closed = false

    fun update(value: Advertisement) { main.post {
        if (!closed && value != desired) { desired = value; reconcile() }
    } }
    /** Registrations are scoped to a Network from API 33; older NSD ignores it. */
    fun network(value: Network?) { main.post {
        if (!closed && Build.VERSION.SDK_INT >= 33 && value != network) { network = value; reconcile() }
    } }
    fun close() { main.post { closed = true; desired = null; reconcile() } }
    private fun reconcile() {
        if (registering || unregistering) return
        val existing = listener
        if (existing != null) {
            if (current != desired || registeredNetwork != network) {
                unregistering = true
                runCatching { nsd.unregisterService(existing) }.onFailure {
                    unregistering = false
                    failure(it.message ?: "停止广播失败")
                }
            }
            return
        }
        val advert = desired ?: return
        val info = NsdServiceInfo().apply {
            serviceName = advert.instance
            serviceType = advert.serviceType
            port = advert.port
            advert.txt.forEach { (key, value) -> setAttribute(key, value) }
            if (Build.VERSION.SDK_INT >= 33) setNetwork(network)
        }
        val registration = object : NsdManager.RegistrationListener {
            override fun onServiceRegistered(info: NsdServiceInfo) { main.post {
                android.util.Log.i("OinkNetwork", "nsd_registered port=${advert.port}")
                registering = false
                reconcile()
            } }
            override fun onRegistrationFailed(info: NsdServiceInfo, code: Int) { main.post {
                registering = false; listener = null; current = null
                failure("设备广播失败：$code")
                // A changed owner snapshot or Network is new work, not a timed retry of a failed registration.
                if (desired != advert || network != registeredNetwork) reconcile()
            } }
            override fun onServiceUnregistered(info: NsdServiceInfo) { main.post {
                unregistering = false; listener = null; current = null
                reconcile()
            } }
            override fun onUnregistrationFailed(info: NsdServiceInfo, code: Int) { main.post {
                unregistering = false
                failure("停止设备广播失败：$code")
            } }
        }
        listener = registration
        current = advert
        registeredNetwork = network
        registering = true
        runCatching { nsd.registerService(info, NsdManager.PROTOCOL_DNS_SD, registration) }.onFailure {
            registering = false; listener = null; current = null
            failure(it.message ?: "设备广播失败")
        }
    }
}
