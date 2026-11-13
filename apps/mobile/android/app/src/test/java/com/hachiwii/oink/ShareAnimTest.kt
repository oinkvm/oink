package com.hachiwii.oink.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** The stage maths against the key frames of share-anim SPEC §5.4, §5.8 and §5.9 (within 0.1, opacities 0.005). */
class ShareAnimTest {
    private fun near(expected: Double, actual: Float, tolerance: Double = .1) = assertEquals(expected, actual.toDouble(), tolerance)

    @Test fun computerKeyFrames() {
        ShareAnim.frame(.30f, ShareTarget.COMPUTER).run {
            near(139.4, p.x); near(28.2, p.y); near(9.1, angle); near(1.0, scale, .005); near(1.0, opacity, .005); near(68.4, trailLength)
        }
        ShareAnim.frame(.50f, ShareTarget.COMPUTER).run { near(183.8, p.x); near(43.0, p.y); near(26.3, angle); near(.586, scale, .005) }
        ShareAnim.frame(.66f, ShareTarget.COMPUTER).run { near(.675, opacity, .005); near(1.025, nudge, .005) }
    }

    @Test fun phoneKeyFrames() {
        ShareAnim.frame(.30f, ShareTarget.PHONE).run { near(141.0, p.x); near(28.8, p.y); near(9.5, angle); near(69.9, trailLength) }
        ShareAnim.frame(.58f, ShareTarget.PHONE).run { near(194.7, p.x); near(48.1, p.y); near(28.1, angle); near(.551, scale, .005) }
        ShareAnim.frame(.72f, ShareTarget.PHONE).run { near(199.0, p.x); near(50.5, p.y); near(29.3, angle); near(1.048, nudge, .005) }
    }

    @Test fun reducedMotionPoseIsTheTopOfTheArc() {
        ShareAnim.frame(ShareAnim.STATIC_T, ShareTarget.COMPUTER).run { near(120.25, p.x); near(26.75, p.y); near(-.41, angle) }
        ShareAnim.frame(ShareAnim.STATIC_T, ShareTarget.PHONE).run { near(121.25, p.x); near(27.13, p.y); near(.20, angle) }
    }

    @Test fun handoffKeyValues() {
        ShareAnim.handoff(0f).run { near(1.0, offline, .005); near(1.0, logo, .005); near(0.0, bar, .005); assertNull(t) }
        ShareAnim.handoff(80f).run { near(.675, offline, .005); near(1.0, logo, .005); near(.533, bar, .005); assertNull(t) }
        ShareAnim.handoff(120f).run { near(.370, offline, .005); near(.800, bar, .005) }
        ShareAnim.handoff(160f).run { near(0.0, offline, .005); near(1.0, logo, .005); near(1.0, bar, .005); near(0.0, t!!, .001) }
        ShareAnim.handoff(200f).run { near(.373, logo, .005); near(.029, t!!, .001) }
        ShareAnim.handoff(230f).run { near(.161, logo, .005); near(.050, t!!, .001) }
        ShareAnim.handoff(300f).run { near(0.0, logo, .005); near(.100, t!!, .001) }
    }
}
