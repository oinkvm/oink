package com.hachiwii.oink.recovery

import org.junit.Assert.*
import org.junit.Test

/**
 * The pairing notification takes the code while Settings stays open (the system closes its pairing
 * dialog, and stops the pairing server, as soon as Settings goes to the background).
 */
class PairingNoticeTest {
    private val idle = PairingCodeState.Idle

    @Test fun offersTheReplyFieldOnceThePairingServiceIsFound() {
        assertNull(pairingNoticeFor(searching = true, port = null, state = idle))
        assertEquals(PairingNotice.Input(41673, retry = false), pairingNoticeFor(true, 41673, idle))
        // Left 第 3 步: nothing is offered.
        assertNull(pairingNoticeFor(searching = false, port = 41673, state = idle))
    }

    @Test fun followsACodeSentFromTheNotification() {
        assertEquals(PairingNotice.Pairing, pairingNoticeFor(false, null, PairingCodeState.Pairing(viaNotice = true)))
        assertEquals(PairingNotice.Paired, pairingNoticeFor(false, null, PairingCodeState.Paired(viaNotice = true)))
        val failed = PairingCodeState.Failed(PairingFailure.CODE_REJECTED, 41673, viaNotice = true)
        assertEquals(PairingNotice.Failed(failed.message), pairingNoticeFor(true, 41673, failed))
    }

    @Test fun aNewCodeAfterAFailureCanBeTypedInTheNotificationAgain() {
        val failed = PairingCodeState.Failed(PairingFailure.CODE_REJECTED, 41673, viaNotice = true)
        // The user stays in Settings and asks for a new code: keep looking for the new service.
        assertTrue(pairingSearching(stepActive = false, state = failed))
        assertEquals(PairingNotice.Input(39827, retry = true), pairingNoticeFor(true, 39827, failed))
    }

    @Test fun theAppSharesProgressButShowsItsOwnFinalResults() {
        assertEquals(PairingNotice.Pairing, pairingNoticeFor(false, null, PairingCodeState.Pairing(viaNotice = false)))
        assertNull(pairingNoticeFor(false, null, PairingCodeState.Paired(viaNotice = false)))
        val failed = PairingCodeState.Failed(PairingFailure.NO_SERVICE, null, viaNotice = false)
        assertFalse(pairingSearching(stepActive = false, state = failed))
        assertNull(pairingNoticeFor(false, null, failed))
    }

    @Test fun leavingTheCodeStepCannotClearTheAcknowledgementBeforeIoStarts() {
        assertEquals(PairingNotice.Pairing, pairingNoticeFor(false, null, idle, submissionPending = true))
        assertNull(pairingNoticeFor(false, null, idle, submissionPending = false))
    }

    @Test fun searchingStopsWhenTheStepIsLeftAndNothingIsPending() {
        assertTrue(pairingSearching(stepActive = true, state = idle))
        assertFalse(pairingSearching(stepActive = false, state = idle))
        assertFalse(pairingSearching(stepActive = false, state = PairingCodeState.Paired(viaNotice = true)))
    }
}
