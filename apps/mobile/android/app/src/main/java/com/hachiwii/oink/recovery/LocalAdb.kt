package com.hachiwii.oink.recovery

import android.content.Context
import java.net.InetAddress
import java.net.NetworkInterface

/** NSD is a port hint. Rust owns TLS/SPAKE2/ADB; Android proves the endpoint is local. */
internal object LocalAdb {
    suspend fun startHelper(context: Context, host: InetAddress, port: Int, credentials: List<ByteArray>) {
        require(isLocal(host) && port in 1..65535)
        if (!RecoveryStore(context).enabled) return
        uniffi.oink_mobile_ffi.adbStartLocal(numericAddress(host), port.toUShort(), credentials,
            context.applicationInfo.sourceDir)
    }
    suspend fun pair(host: InetAddress, port: Int, code: String, credential: ByteArray) {
        require(isLocal(host) && port in 1..65535 && code.matches(Regex("[0-9]{6}")))
        uniffi.oink_mobile_ffi.adbPairLocal(numericAddress(host), port.toUShort(), code, credential)
    }
    private fun numericAddress(address: InetAddress): String {
        val host = requireNotNull(address.hostAddress).substringBefore('%')
        return if (address is java.net.Inet6Address && address.scopeId != 0) "$host%${address.scopeId}" else host
    }
    fun publicKeyLine(credential: ByteArray) = uniffi.oink_mobile_ffi.adbPublicKey(credential)
    fun isLocal(address: InetAddress): Boolean = address.isLoopbackAddress || runCatching {
        NetworkInterface.getNetworkInterfaces()?.toList().orEmpty().any { address in it.inetAddresses.toList() }
    }.getOrDefault(false)
}
