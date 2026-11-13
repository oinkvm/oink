package com.hachiwii.oink.recovery

import org.junit.Assert.*
import org.junit.Test
import uniffi.oink_mobile_ffi.RecoveryKey

class RecoveryKeySetTest {
    private fun key(owner: String, revision: ULong = 1u, revoked: Boolean = false, id: Byte = 1) =
        RecoveryKey(owner, ByteArray(16) { id }, owner, revision, revoked, byteArrayOf(), byteArrayOf())
    @Test fun concurrentCreatorsConvergeWithoutReplacingEachOther() {
        val a = RecoveryKeySet(0u, "aa")
        val b = RecoveryKeySet(0u, "bb")
        a.merge(0u, "aa", listOf(key("aa")))
        b.merge(0u, "bb", listOf(key("bb")))
        a.merge(0u, "bb", b.records.values.toList())
        b.merge(0u, "aa", a.records.values.toList())
        assertEquals(setOf("aa", "bb"), a.records.keys)
        assertEquals(a.records.keys, b.records.keys)
    }
    @Test fun sameRevisionRevocationWinsAndDuplicateDoesNotChange() {
        val state = RecoveryKeySet(0u, "aa")
        state.merge(0u, "aa", listOf(key("aa")))
        assertTrue(state.merge(0u, "aa", listOf(key("aa", revoked = true))))
        assertFalse(state.merge(0u, "bb", listOf(key("aa"))))
        assertTrue(state.records.getValue("aa").revoked)
    }
    @Test fun resetAndOldOfflineDeliveryCannotResurrectKey() {
        val state = RecoveryKeySet(0u, "aa")
        state.merge(0u, "aa", listOf(key("aa")))
        state.reset("bb")
        assertFalse(state.merge(0u, "aa", listOf(key("aa", 100u))))
        assertTrue(state.records.isEmpty())
    }
    @Test fun concurrentResetUsesOriginAndMissingRecordNeverDeletes() {
        val state = RecoveryKeySet(1u, "bb")
        state.merge(1u, "bb", listOf(key("aa")))
        assertFalse(state.merge(1u, "aa", emptyList()))
        assertFalse(state.merge(1u, "bb", emptyList()))
        assertTrue(state.merge(1u, "cc", emptyList()))
        assertTrue(state.records.isEmpty())
    }
    @Test fun memberRemovalCreatesNewerTombstone() {
        val state = RecoveryKeySet(0u, "aa")
        state.merge(0u, "aa", listOf(key("bb", 3u)))
        assertTrue(state.revokeRemoved(setOf("bb"), "aa"))
        assertEquals(4uL, state.records.getValue("bb").revision)
        assertTrue(state.records.getValue("bb").revoked)
        assertFalse(state.merge(0u, "bb", listOf(key("bb", 3u))))
    }
    @Test fun equalRevisionOriginAndUnsignedKeyIdConverge() {
        val low = key("aa").copy(origin = "bb")
        val high = key("aa", id = 0xff.toByte()).copy(origin = "cc")
        val state = RecoveryKeySet(0u, "aa")
        assertTrue(state.merge(0u, "aa", listOf(high)))
        assertFalse(state.merge(0u, "aa", listOf(low)))
        assertFalse(state.merge(0u, "aa", listOf(high.copy(keyId = ByteArray(16)))))
        assertEquals("cc", state.records.getValue("aa").origin)
    }
    @Test fun credentialBeforeRosterDoesNotPoisonLaterDelivery() {
        val phone = RecoveryKeySet(0u, "aa")
        val pc = RecoveryKeySet(0u, "bb")
        pc.merge(0u, "bb", listOf(key("bb")))
        assertFalse(phone.merge(0u, "bb", pc.records.values.toList(), setOf("aa")))
        assertTrue(phone.records.isEmpty()) // No synthetic tombstone to spread back to the PC.
        assertFalse(pc.merge(0u, "aa", phone.records.values.toList(), setOf("aa", "bb")))
        assertTrue(phone.merge(0u, "bb", pc.records.values.toList(), setOf("aa", "bb")))
        assertFalse(phone.records.getValue("bb").revoked)
    }
    @Test fun currentRosterAcceptsNewMemberBeforeCachedRosterRefresh() {
        val phone = RecoveryKeySet(0u, "aa")
        val previous = setOf("aa")
        val current = setOf("aa", "bb")
        assertTrue(phone.merge(0u, "bb", listOf(key("bb")), current))
        assertFalse(phone.revokeRemoved(previous - current, "aa"))
        assertFalse(phone.records.getValue("bb").revoked)
        assertEquals(1uL, phone.records.getValue("bb").revision)
    }
    @Test fun formerMemberTombstonesRemainMergeable() {
        val phone = RecoveryKeySet(0u, "aa")
        assertTrue(phone.merge(0u, "bb", listOf(key("bb", revoked = true)), setOf("aa")))
        assertTrue(phone.records.getValue("bb").revoked)
    }
    @Test fun explicitNewerGenerationCanReplaceTombstone() {
        val state = RecoveryKeySet(0u, "aa")
        state.merge(0u, "aa", listOf(key("aa", 7u, revoked = true)))
        assertTrue(state.merge(0u, "aa", listOf(key("aa", 8u, id = 2))))
        assertFalse(state.records.getValue("aa").revoked)
        assertEquals(8uL, state.records.getValue("aa").revision)
    }
}
