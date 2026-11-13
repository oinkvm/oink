package com.hachiwii.oink.recovery

/** Counters survive group leave/reset, so stale peer state cannot overwrite a newer local choice. */
internal object RecoveryRevision {
    fun next(current: ULong, retained: ULong = 0u): ULong {
        val high = maxOf(current, retained)
        check(high != ULong.MAX_VALUE) { "revision_exhausted" }
        return high + 1u
    }
}

data class RecoveryIntent(val enabled: Boolean, val revision: String) {
    internal fun updated(enabled: Boolean) = RecoveryIntent(enabled, RecoveryRevision.next(revision.toULong()).toString())
    internal fun disableIfRevision(expected: String): RecoveryIntent? =
        if (revision == expected) updated(false) else null
}
