package com.hachiwii.oink.recovery

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import org.json.JSONArray
import org.json.JSONObject
import uniffi.oink_mobile_ffi.RecoveryCredentials
import uniffi.oink_mobile_ffi.RecoveryKey
import java.security.KeyFactory
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.interfaces.RSAPrivateCrtKey
import java.security.spec.PKCS8EncodedKeySpec
import java.util.Base64
import java.util.UUID
import java.nio.ByteBuffer
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** DE contains explicit intent and an AES-GCM wrapped ADB set, never group identity secrets. */
class RecoveryStore(context: Context) {
    private val prefs = context.createDeviceProtectedStorageContext()
        .getSharedPreferences("privilege_recovery", Context.MODE_PRIVATE)
    var enabled: Boolean
        get() = synchronized(lock) { prefs.getBoolean("enabled", false) }
        set(value) { synchronized(lock) { writeIntent(intentSnapshot().updated(value)) } }
    /** Decimal unsigned counter, exposed without Kotlin unsigned name mangling to the Provider. */
    val intentRevision: String get() = synchronized(lock) { prefs.getString("intent_revision", "0")!! }
    fun intentSnapshot(): RecoveryIntent = synchronized(lock) { RecoveryIntent(enabled, intentRevision) }
    private fun writeIntent(value: RecoveryIntent) {
        check(prefs.edit().putBoolean("enabled", value.enabled).putString("intent_revision", value.revision).commit())
    }
    fun compareAndDisable(expectedRevision: String): Boolean = synchronized(lock) {
        val next = intentSnapshot().disableIfRevision(expectedRevision) ?: return@synchronized false
        writeIntent(next)
        true
    }
    var connectionWanted: Boolean
        get() = prefs.getBoolean("connection_wanted", false)
        set(value) { check(prefs.edit().putBoolean("connection_wanted", value).commit()) }
    val hasCredential: Boolean get() = credentials().isNotEmpty()
    val localDevice: String get() = checkNotNull(prefs.getString("local", null)) { "group_required" }
    private fun wrappingKey(): SecretKey {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (store.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
            init(KeyGenParameterSpec.Builder(KEY_ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setUserAuthenticationRequired(false).setUnlockedDeviceRequired(false).build())
        }.generateKey()
    }
    private fun read(): RecoveryKeySet {
        val encoded = prefs.getString("set", null) ?: return RecoveryKeySet(0u, prefs.getString("local", "")!!)
        val blob = Base64.getDecoder().decode(encoded)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, wrappingKey(), GCMParameterSpec(128, blob.copyOfRange(0, 12)))
        val plain = cipher.doFinal(blob, 12, blob.size - 12)
        val json = try { JSONObject(plain.toString(Charsets.UTF_8)) } finally { plain.fill(0) }
        val state = RecoveryKeySet(json.getString("counter").toULong(), json.getString("origin"))
        val records = json.getJSONArray("records")
        for (index in 0 until records.length()) {
            val value = records.getJSONObject(index)
            val record = RecoveryKey(value.getString("owner"), decode(value.getString("key")),
                value.getString("origin"), value.getString("revision").toULong(), value.getBoolean("revoked"),
                decode(value.getString("private")), decode(value.getString("public")))
            state.records[record.owner] = record
        }
        return state
    }
    private fun write(state: RecoveryKeySet) {
        val records = JSONArray()
        state.records.values.forEach { value -> records.put(JSONObject()
            .put("owner", value.owner).put("key", encode(value.keyId)).put("origin", value.origin)
            .put("revision", value.revision.toString()).put("revoked", value.revoked)
            .put("private", encode(value.privateKey)).put("public", encode(value.publicKey))) }
        val json = JSONObject().put("counter", state.epochCounter.toString()).put("origin", state.epochOrigin).put("records", records)
        val plain = json.toString().toByteArray()
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, wrappingKey())
        val encrypted = try { cipher.iv + cipher.doFinal(plain) } finally { plain.fill(0) }
        val editor = prefs.edit().putString("set", encode(encrypted))
        val local = prefs.getString("local", null)
        state.records[local]?.let { record ->
            val name = "key_revision:$local"
            val high = prefs.getString(name, "0")!!.toULong()
            editor.putString(name, maxOf(high, record.revision).toString())
        }
        check(editor.commit())
    }
    fun bindGroup(group: ByteArray, local: String, members: () -> List<String>) = synchronized(lock) {
        val id = encode(group)
        if (prefs.getString("group", null) != id) check(prefs.edit().remove("set").putString("group", id).commit())
        val current = members().toSet() + local
        val previous = prefs.getStringSet("members", emptySet())!!.toSet()
        val state = read()
        if (state.revokeRemoved(previous - current, local)) write(state)
        check(prefs.edit().putString("local", local).putStringSet("members", current).commit())
    }
    fun merge(value: RecoveryCredentials, members: () -> List<String>): Boolean = synchronized(lock) {
        require(encode(value.group) == prefs.getString("group", null)) { "stale_group" }
        // Query the live owner under the store lock, not the previous bindGroup cache.
        val current = members().toSet() + localDevice
        val retained = prefs.getString("key_revision:$localDevice", "0")!!.toULong()
        val incoming = value.records.filter { record ->
            record.revoked || (record.owner in current &&
                (record.owner != localDevice || record.revision >= retained))
        }
        incoming.forEach { record ->
            require(record.keyId.size == 16)
            if (!record.revoked) {
                parseKey(record.privateKey)
                require(LocalAdb.publicKeyLine(record.privateKey).toString(Charsets.US_ASCII).substringBefore(" ") == record.publicKey.toString(Charsets.US_ASCII).substringBefore(" "))
            }
        }
        val state = read()
        var changed = state.merge(value.epochCounter, value.epochOrigin, incoming, current)
        val previous = prefs.getStringSet("members", emptySet())!!.toSet()
        changed = state.revokeRemoved(previous - current, localDevice) || changed
        if (changed) write(state)
        check(prefs.edit().putStringSet("members", current).commit())
        changed
    }
    fun snapshot(): RecoveryCredentials = synchronized(lock) {
        val state = read()
        RecoveryCredentials(decode(checkNotNull(prefs.getString("group", null))), localDevice, randomId(), state.epochCounter, state.epochOrigin, state.records.values.toList())
    }
    fun credentials(): List<ByteArray> = synchronized(lock) {
        read().records.values.filter { !it.revoked }.map { it.privateKey }
    }
    /** Each member generates at most its own record; reset/revocation requires a newer revision. */
    fun pairingCredential(): ByteArray = synchronized(lock) {
        val state = read()
        val retained = prefs.getString("key_revision:$localDevice", "0")!!.toULong()
        state.records[localDevice]?.takeIf { !it.revoked && it.revision >= retained }
            ?.let { return@synchronized it.privateKey }
        val generated = KeyPairGenerator.getInstance("RSA").apply { initialize(2048) }.generateKeyPair()
        val pem = ("-----BEGIN PRIVATE KEY-----\n" + Base64.getMimeEncoder(64, byteArrayOf(10)).encodeToString(generated.private.encoded) +
            "\n-----END PRIVATE KEY-----\n").toByteArray(Charsets.US_ASCII)
        val revision = RecoveryRevision.next(state.records[localDevice]?.revision ?: 0uL,
            retained)
        val publicKey = LocalAdb.publicKeyLine(pem)
        state.records[localDevice] = RecoveryKey(localDevice, randomId(), localDevice, revision, false, pem, publicKey)
        write(state)
        pem
    }
    fun reset() = synchronized(lock) { val state = read(); state.reset(localDevice); write(state) }
    fun clear() = synchronized(lock) {
        // Retain only public monotonic counters across leave; no private key or group binding survives.
        val counters = prefs.all.filterKeys { it == "intent_revision" || it.startsWith("key_revision:") }.toMutableMap()
        prefs.getString("local", null)?.let { local ->
            val name = "key_revision:$local"
            val high = (counters[name] as? String ?: "0").toULong()
            // Leave revokes the old generation even if an offline peer sends it after rejoining.
            counters[name] = RecoveryRevision.next(high, read().records[local]?.revision ?: 0uL).toString()
        }
        val editor = prefs.edit().clear()
        counters.forEach { (key, value) -> editor.putString(key, value as String) }
        check(editor.commit())
        KeyStore.getInstance("AndroidKeyStore").apply { load(null); deleteEntry(KEY_ALIAS) }
    }
    companion object {
        private val lock = Any()
        private const val KEY_ALIAS = "oink.adb.recovery.v1"
        private fun encode(bytes: ByteArray) = Base64.getEncoder().encodeToString(bytes)
        private fun decode(text: String) = Base64.getDecoder().decode(text)
        fun randomId(): ByteArray = UUID.randomUUID().let { ByteBuffer.allocate(16).putLong(it.mostSignificantBits).putLong(it.leastSignificantBits).array() }
        fun parseKey(pem: ByteArray): RSAPrivateCrtKey {
            val text = pem.toString(Charsets.US_ASCII).trim()
            require(text.startsWith("-----BEGIN PRIVATE KEY-----") && text.endsWith("-----END PRIVATE KEY-----"))
            val body = text.removePrefix("-----BEGIN PRIVATE KEY-----").removeSuffix("-----END PRIVATE KEY-----").filterNot(Char::isWhitespace)
            val key = KeyFactory.getInstance("RSA").generatePrivate(PKCS8EncodedKeySpec(decode(body))) as RSAPrivateCrtKey
            require(key.modulus.bitLength() == 2048) { "ADB requires RSA-2048" }
            return key
        }
    }
}
