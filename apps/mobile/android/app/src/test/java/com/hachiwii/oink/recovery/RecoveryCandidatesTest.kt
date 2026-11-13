package com.hachiwii.oink.recovery

import org.junit.Assert.*
import org.junit.Test

class RecoveryCandidatesTest {
    @Test fun failuresAndDuplicatesDoNotHideLaterAuthenticEndpoint() {
        val queue = ArrayDeque(listOf("fake1", "fake2", "fake1", "fake3", "fake4", "authentic"))
        val tried = mutableSetOf<String>()
        repeat(4) { assertEquals("fake${it + 1}", nextRecoveryCandidate(queue, tried) { it }) }
        assertEquals("authentic", nextRecoveryCandidate(queue, tried) { it })
        assertNull(nextRecoveryCandidate(queue, tried) { it })
    }
    @Test fun laterDiscoveryIsTriedEvenAfterEarlierFailures() {
        val queue = ArrayDeque((1..4).map { "fake$it" })
        val tried = mutableSetOf<String>()
        repeat(4) { nextRecoveryCandidate(queue, tried) { it } }
        queue.add("authentic")
        assertEquals("authentic", nextRecoveryCandidate(queue, tried) { it })
    }
}
