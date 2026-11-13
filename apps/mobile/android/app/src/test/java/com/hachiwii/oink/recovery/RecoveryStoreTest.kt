package com.hachiwii.oink.recovery

import android.app.Application
import android.content.Context
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/** Persistence and user intent. Device pairing tests exercise Android Keystore and ADB itself. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = Application::class)
class RecoveryStoreTest {
    private val context get() = RuntimeEnvironment.getApplication()
    private val prefs get() = context.createDeviceProtectedStorageContext()
        .getSharedPreferences("privilege_recovery", Context.MODE_PRIVATE)

    @Before fun clear() { prefs.edit().clear().commit() }

    @Test fun reopeningKeepsTheLocalCredentialAndIntent() {
        prefs.edit().putString("credential", "encrypted-local-key").commit()
        RecoveryStore(context).enabled = true
        val reopened = RecoveryStore(context)
        assertTrue(reopened.enabled)
        assertTrue(reopened.hasCredential)
        assertEquals("encrypted-local-key", prefs.getString("credential", null))
    }

    @Test fun staleGroupMetadataCannotDiscardALocalCredential() {
        prefs.edit().putString("credential", "encrypted-local-key").putString("group", "standalone").commit()
        RecoveryStore(context).enabled = true
        prefs.edit().putString("group", "joined-group").commit()
        assertTrue(RecoveryStore(context).hasCredential)
        prefs.edit().remove("group").commit()
        assertTrue(RecoveryStore(context).hasCredential)
        assertTrue(RecoveryStore(context).enabled)
    }

    @Test fun disablingErasesTheCredentialAndSurvivesReopening() {
        prefs.edit().putString("credential", "encrypted-local-key").commit()
        val store = RecoveryStore(context)
        store.enabled = true
        store.enabled = false
        assertFalse(RecoveryStore(context).enabled)
        assertFalse(RecoveryStore(context).hasCredential)
        assertFalse(prefs.contains("credential"))
    }

    @Test fun obsoleteGroupRecordsRequireOneNewPairing() {
        prefs.edit().putBoolean("enabled", true).putString("set", "obsolete-encrypted-records")
            .putString("group", "old-group").commit()
        val store = RecoveryStore(context)
        assertFalse(store.enabled)
        assertFalse(store.hasCredential)
        assertFalse(prefs.contains("set"))
        assertFalse(prefs.contains("group"))
    }
}
