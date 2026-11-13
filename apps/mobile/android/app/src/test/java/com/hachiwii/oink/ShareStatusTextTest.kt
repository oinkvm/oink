package com.hachiwii.oink

import org.junit.Assert.assertEquals
import org.junit.Test

class ShareStatusTextTest {
    @Test fun sendingWithoutSampleOmitsRate() {
        assertEquals("正在发送…", ShareStatusText.sending(null))
    }

    @Test fun sendingFormatsKilobytesWithThreeSignificantDigits() {
        assertEquals("正在发送… 0.512 KB/s", ShareStatusText.sending(512.0))
    }

    @Test fun sendingPromotesAfterRoundingReachesUnitBoundary() {
        assertEquals("正在发送… 1.00 MB/s", ShareStatusText.sending(999_600.0))
    }

    @Test fun sendingFormatsGigabytes() {
        assertEquals("正在发送… 1.23 GB/s", ShareStatusText.sending(1_230_000_000.0))
    }

    @Test fun completedFormatsElapsedBoundaryValues() {
        assertEquals("已完成，耗时 0.420 秒", ShareStatusText.completed(0.420))
        assertEquals("已完成，耗时 3.18 秒", ShareStatusText.completed(3.18))
        assertEquals("已完成，耗时 12.5 秒", ShareStatusText.completed(12.5))
        assertEquals("已完成，耗时 128 秒", ShareStatusText.completed(128.0))
    }
}
