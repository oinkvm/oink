package com.hachiwii.oink.recovery

import uniffi.oink_mobile_ffi.RecoveryKey

/** Frozen protocol merge order. Device keys are canonical lowercase public-key hex. */
internal class RecoveryKeySet(
    var epochCounter: ULong,
    var epochOrigin: String,
    val records: MutableMap<String, RecoveryKey> = sortedMapOf(),
) {
    fun merge(counter: ULong, origin: String, incoming: List<RecoveryKey>, members: Set<String>? = null): Boolean {
        val epochOrder = compareValues(counter, epochCounter).takeIf { it != 0 }
            ?: if (counter == 0uL) 0 else origin.compareTo(epochOrigin)
        if (epochOrder < 0) return false
        var changed = epochOrder > 0
        if (changed) { records.clear(); epochCounter = counter; epochOrigin = origin }
        for (value in incoming) {
            // A credential can precede the roster update. Never turn that race into a revocation.
            if (!value.revoked && members != null && value.owner !in members) continue
            val old = records[value.owner]
            if (old == null || compare(value, old) > 0) { records[value.owner] = value; changed = true }
        }
        return changed
    }
    fun revokeRemoved(removed: Set<String>, local: String): Boolean {
        var changed = false
        for ((owner, key) in records.toMap()) {
            if (!key.revoked && owner in removed) {
                check(key.revision != ULong.MAX_VALUE)
                records[owner] = RecoveryKey(owner, key.keyId, local, key.revision + 1u,
                    true, byteArrayOf(), byteArrayOf())
                changed = true
            }
        }
        return changed
    }
    fun reset(local: String) {
        check(epochCounter != ULong.MAX_VALUE)
        epochCounter++
        epochOrigin = local
        records.clear()
    }
    companion object {
        private fun compare(a: RecoveryKey, b: RecoveryKey): Int {
            compareValues(a.revision, b.revision).let { if (it != 0) return it }
            compareValues(a.revoked, b.revoked).let { if (it != 0) return it }
            a.origin.compareTo(b.origin).let { if (it != 0) return it }
            for (index in a.keyId.indices) {
                compareValues(a.keyId[index].toUByte(), b.keyId[index].toUByte()).let { if (it != 0) return it }
            }
            return 0
        }
    }
}
