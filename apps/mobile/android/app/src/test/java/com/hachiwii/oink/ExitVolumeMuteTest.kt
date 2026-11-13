package com.hachiwii.oink

import com.hachiwii.oink.audio.reportedIndex
import com.hachiwii.oink.audio.unmuteIndex
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * The phone's media stream as a PC's exit volume while it is muted (2026-11-12): Android reports 0
 * from getStreamVolume() for a muted stream, and a PC's unmute used to write that 0 back.
 */
class ExitVolumeMuteTest {
    @Test fun aMutedStreamReportsItsLastAudibleIndexNotTheZeroAndroidReturns() {
        assertEquals(75, reportedIndex(muted = true, streamVolume = 0, audibleIndex = 75))
        assertEquals(40, reportedIndex(muted = false, streamVolume = 40, audibleIndex = 75))
        // Muted before this process saw it audible: the only reading there is.
        assertEquals(0, reportedIndex(muted = true, streamVolume = 0, audibleIndex = null))
    }

    @Test fun unmutingWithTheReportedZeroOfAnUnknownLevelKeepsTheIndexAndroidRestores() {
        // The 01:59:54 case on the test phone: muted at 75 by a PC, unmuted with 0 %.
        assertEquals(75, unmuteIndex(target = 0, minimum = 0, audibleIndex = null, restored = 75))
        // A level the PC chose is written as asked.
        assertEquals(30, unmuteIndex(target = 30, minimum = 0, audibleIndex = null, restored = 75))
        // With a known audible index the PC was shown it; its request is the user's level, 0 included.
        assertEquals(0, unmuteIndex(target = 0, minimum = 0, audibleIndex = 75, restored = 75))
        assertEquals(75, unmuteIndex(target = 75, minimum = 0, audibleIndex = 75, restored = 75))
    }
}
