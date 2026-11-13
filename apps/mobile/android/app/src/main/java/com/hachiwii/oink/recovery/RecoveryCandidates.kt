package com.hachiwii.oink.recovery

/** NSD is untrusted: deduplicate endpoints without letting failures exhaust a global budget. */
internal fun <T> nextRecoveryCandidate(queue: ArrayDeque<T>, tried: MutableSet<String>, endpoint: (T) -> String): T? {
    while (queue.isNotEmpty()) {
        val value = queue.removeFirst()
        if (tried.add(endpoint(value))) return value
    }
    return null
}
