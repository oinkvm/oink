package com.hachiwii.oink.recovery

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test

/**
 * 特权模式第 3 步 (Phone report 2026-11-12): a failed pairing can be retried, the code from the app's
 * field and from the notification go through the same single attempt, and a vanished pairing service
 * fails with its own reason instead of a generic one.
 */
class PairingCodeFlowTest {
    private class Fake(var port: Int?, var result: PairingFailure? = null) {
        val calls = mutableListOf<Pair<Int, String>>()
        var gate: CompletableDeferred<Unit>? = null
        val flow = PairingCodeFlow({ port }) { p, c -> calls += p to c; gate?.await(); result }
    }

    @Test fun noPairingServiceFailsWithoutDialing() = runBlocking {
        val fake = Fake(port = null)
        assertTrue(fake.flow.submit("123456", viaNotice = false))
        assertEquals(PairingCodeState.Failed(PairingFailure.NO_SERVICE, null, false), fake.flow.state.value)
        assertTrue(fake.calls.isEmpty())
    }

    @Test fun pairsWithTheFoundPortAndCode() = runBlocking {
        val fake = Fake(port = 41673)
        assertTrue(fake.flow.submit("654321", viaNotice = true))
        assertEquals(listOf(41673 to "654321"), fake.calls)
        assertEquals(PairingCodeState.Paired(viaNotice = true), fake.flow.state.value)
    }

    @Test fun onlySixDigitsAreSent() = runBlocking {
        val fake = Fake(port = 41673)
        assertFalse(fake.flow.submit("12345", viaNotice = true))
        assertFalse(fake.flow.submit("12a456", viaNotice = true))
        assertEquals(PairingCodeState.Idle, fake.flow.state.value)
        assertTrue(fake.calls.isEmpty())
    }

    @Test fun aClosedSystemDialogSaysSoAndPointsToTheNotification() = runBlocking {
        val fake = Fake(port = 41673, result = PairingFailure.NO_SERVICE)
        fake.flow.submit("123456", viaNotice = false)
        val failed = fake.flow.state.value as PairingCodeState.Failed
        assertEquals(41673, failed.port)
        assertTrue(failed.message, failed.message.contains("离开「设置」") && failed.message.contains("通知"))
    }

    @Test fun failureThenBackThenRetrySucceeds() = runBlocking {
        val fake = Fake(port = 41673, result = PairingFailure.CODE_REJECTED)
        fake.flow.submit("111111", viaNotice = false)
        assertTrue(fake.flow.state.value is PairingCodeState.Failed)
        // 「重新配对」 goes back to the code step: the failure is forgotten and a new code is accepted.
        fake.flow.reset()
        assertEquals(PairingCodeState.Idle, fake.flow.state.value)
        fake.port = 39827; fake.result = null
        assertTrue(fake.flow.submit("222222", viaNotice = false))
        assertEquals(listOf(41673 to "111111", 39827 to "222222"), fake.calls)
        assertEquals(PairingCodeState.Paired(viaNotice = false), fake.flow.state.value)
    }

    @Test fun aFailureCanAlsoBeRetriedDirectlyWithANewCode() = runBlocking {
        val fake = Fake(port = 41673, result = PairingFailure.CODE_REJECTED)
        fake.flow.submit("111111", viaNotice = true)
        fake.port = 39827; fake.result = null
        assertTrue(fake.flow.submit("222222", viaNotice = true))
        assertEquals(PairingCodeState.Paired(viaNotice = true), fake.flow.state.value)
    }

    @Test fun oneAttemptAtATime() = runBlocking {
        val fake = Fake(port = 41673).apply { gate = CompletableDeferred() }
        val first = async(start = CoroutineStart.UNDISPATCHED) { fake.flow.submit("111111", viaNotice = true) }
        assertEquals(PairingCodeState.Pairing(viaNotice = true), fake.flow.state.value)
        // The app's field while the notification's code is being paired, and 「重新配对」 meanwhile.
        assertFalse(fake.flow.submit("222222", viaNotice = false))
        fake.flow.reset()
        assertEquals(PairingCodeState.Pairing(viaNotice = true), fake.flow.state.value)
        fake.gate!!.complete(Unit)
        assertTrue(first.await())
        assertEquals(listOf(41673 to "111111"), fake.calls)
        assertEquals(PairingCodeState.Paired(viaNotice = true), fake.flow.state.value)
    }
}
