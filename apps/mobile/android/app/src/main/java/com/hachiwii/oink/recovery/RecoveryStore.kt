package com.hachiwii.oink.recovery

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.util.Base64
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** This phone's own wireless-ADB key and user intent, independent of any Oink group. */
class RecoveryStore(context: Context) {
    private val prefs = context.createDeviceProtectedStorageContext()
        .getSharedPreferences("privilege_recovery", Context.MODE_PRIVATE)

    init {
        synchronized(lock) {
            // The unpublished group-record format is discarded. A single local key
            // has no group binding or member records, including after leaving a group.
            if (prefs.contains("set")) {
                check(prefs.edit().clear().commit())
                android.util.Log.w("OinkRecovery", "discarded obsolete group-bound ADB credentials; pair locally again")
            }
        }
    }

    var enabled: Boolean
        get() = synchronized(lock) { prefs.getBoolean("enabled", false) }
        set(value) = synchronized(lock) {
            val editor = prefs.edit().putBoolean("enabled", value)
            if (!value) editor.remove(CREDENTIAL)
            check(editor.commit())
        }

    val hasCredential: Boolean get() = synchronized(lock) { prefs.contains(CREDENTIAL) }

    private fun wrappingKey(create: Boolean): SecretKey {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (store.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        check(create) { "local_adb_wrapping_key_unavailable" }
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
            init(KeyGenParameterSpec.Builder(KEY_ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setUserAuthenticationRequired(false).setUnlockedDeviceRequired(false).build())
        }.generateKey()
    }

    private fun read(): ByteArray? {
        val encoded = prefs.getString(CREDENTIAL, null) ?: return null
        val blob = Base64.getDecoder().decode(encoded)
        require(blob.size > 12) { "invalid_local_adb_credential" }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, wrappingKey(false), GCMParameterSpec(128, blob.copyOfRange(0, 12)))
        return cipher.doFinal(blob, 12, blob.size - 12)
    }

    fun credentials(): List<ByteArray> = synchronized(lock) { listOfNotNull(read()) }

    /** The paired key survives process death, app restart, joining and leaving groups. */
    fun pairingCredential(): ByteArray = synchronized(lock) {
        read()?.let { return@synchronized it }
        val generated = KeyPairGenerator.getInstance("RSA").apply { initialize(2048) }.generateKeyPair()
        val pem = ("-----BEGIN PRIVATE KEY-----\n" +
            Base64.getMimeEncoder(64, byteArrayOf(10)).encodeToString(generated.private.encoded) +
            "\n-----END PRIVATE KEY-----\n").toByteArray(Charsets.US_ASCII)
        try {
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.ENCRYPT_MODE, wrappingKey(true))
            val encrypted = cipher.iv + cipher.doFinal(pem)
            check(prefs.edit().putString(CREDENTIAL, Base64.getEncoder().encodeToString(encrypted)).commit())
            pem
        } catch (failure: Exception) {
            pem.fill(0)
            throw failure
        }
    }

    companion object {
        private val lock = Any()
        private const val CREDENTIAL = "credential"
        private const val KEY_ALIAS = "oink.adb.recovery.v1"

    }
}
