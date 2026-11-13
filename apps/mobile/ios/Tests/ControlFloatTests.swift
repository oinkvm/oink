import XCTest
@testable import OinkAudioTestHost

/// The landscape full screen and floating keyboard rules (control-page.md §1.2, user 2026-11-12);
/// the same cases as Android's FloatMotionTest / FullscreenTest.
final class ControlFloatTests: XCTestCase {
    func testDictationPromptKeepsInputUntilBackgroundOrTabExit() {
        XCTAssertTrue(ControlPageExit.keepsInput(tabShown: true, phase: .inactive))
        XCTAssertTrue(ControlPageExit.keepsInput(tabShown: true, phase: .active))
        XCTAssertFalse(ControlPageExit.keepsInput(tabShown: true, phase: .background))
        XCTAssertFalse(ControlPageExit.keepsInput(tabShown: false, phase: .inactive))
    }
    func testTextFocusStillWaitsForPadIdle() {
        XCTAssertEqual(TrackpadRecognizer.touchpadIdleDelay, 1.2)
    }

    func testDragTicksFollowTravelNotTimeOrPointerSpeed() {
        var travel = DragTickTravel()
        XCTAssertEqual(travel.move(CGVector(dx: 3, dy: 4)), 0)
        XCTAssertEqual(travel.move(CGVector(dx: -3, dy: -4)), 0, "backtracking is real travel")
        XCTAssertEqual(travel.move(CGVector(dx: 2, dy: 0)), 1)
        XCTAssertEqual(travel.remainder, 0)
        XCTAssertEqual(travel.move(CGVector(dx: 25, dy: 0)), 2)
        XCTAssertEqual(travel.remainder, 1)
        travel.reset()
        XCTAssertEqual(travel.move(CGVector(dx: 11, dy: 0)), 0, "relay fingers start a new distance")
        XCTAssertEqual(travel.move(.zero), 0, "stationary holds do not tick")
    }

    // iPhone landscape: 852 wide, 59 safe area on both sides.
    private let island = FloatMotion.Bounds(screenWidth: 852, safeLeft: 59, safeRight: 59, islandRight: false)
    private let flipped = FloatMotion.Bounds(screenWidth: 852, safeLeft: 59, safeRight: 59, islandRight: true)

    func testProjectionIsApplesDecelerationRate() {
        // r = 0.998: about 0.499 s of the velocity.
        XCTAssertEqual(FloatMotion.project(100, velocity: 1000), 100 + 499, accuracy: 0.01)
        XCTAssertEqual(FloatMotion.project(0, velocity: -200), -99.8, accuracy: 0.01)
    }

    func testBoundsKeepTheIslandSideSafeAndTheFreeSide8FromTheEdge() {
        // Island on the left: from the safe area on the left to 8 from the right screen edge.
        XCTAssertEqual(island.lo, 59)
        XCTAssertEqual(island.hi, 852 - 8 - 400)
        XCTAssertEqual(island.home(islandRight: false), island.hi, "rests away from the island")
        XCTAssertEqual(flipped.lo, 8)
        XCTAssertEqual(flipped.hi, 852 - 59 - 400)
        XCTAssertEqual(flipped.home(islandRight: true), flipped.lo)
    }

    func testRubberBandPastTheBoundsMovesAThird() {
        XCTAssertEqual(island.rubber(island.hi + 30), island.hi + 10)
        XCTAssertEqual(island.rubber(island.lo - 60), island.lo - 20)
        XCTAssertEqual(island.rubber(200), 200)
    }

    func testFlickRestsAtTheProjectionClampedAndSnapped() {
        // Slow, mid-range: stays where projected.
        XCTAssertEqual(island.rest(250, velocity: 0), 250)
        // Within 24 of a bound: snapped.
        XCTAssertEqual(island.rest(island.lo + 20, velocity: 0), island.lo)
        XCTAssertEqual(island.rest(island.hi - 23, velocity: 0), island.hi)
        // A fast flick past a bound: the bound.
        XCTAssertEqual(island.rest(200, velocity: 3000), island.hi)
        XCTAssertEqual(island.rest(300, velocity: -3000), island.lo)
    }

    func testDismissLookAndThreshold() {
        let half = FloatMotion.dismissLook(down: 80, finish: 0)
        XCTAssertEqual(half.scale, 0.875, accuracy: 0.0001)
        XCTAssertEqual(half.opacity, 0.7, accuracy: 0.0001)
        let full = FloatMotion.dismissLook(down: 160, finish: 0)
        XCTAssertEqual(full.scale, 0.75, accuracy: 0.0001)
        XCTAssertEqual(full.opacity, 0.4, accuracy: 0.0001)
        let done = FloatMotion.dismissLook(down: 80, finish: 1)
        XCTAssertEqual(done.scale, 0.6, accuracy: 0.0001)
        XCTAssertEqual(done.opacity, 0, accuracy: 0.0001)
        XCTAssertTrue(FloatMotion.dismisses(down: 81, velocity: 0), "a slow drag has to travel 80")
        XCTAssertFalse(FloatMotion.dismisses(down: 60, velocity: 0))
        XCTAssertTrue(FloatMotion.dismisses(down: 20, velocity: 200), "a short fast flick dismisses")
    }

    func testMorphLookPassesThroughTheIconOnly() {
        XCTAssertEqual(FloatMotion.morphLook(q: 0), .init(floating: 1, icon: 0))
        XCTAssertEqual(FloatMotion.morphLook(q: 0.7), .init(floating: 0, icon: 1))
    }

    func testSpringIsCriticallyDampedAtResponse035() {
        XCTAssertEqual(FloatMotion.stiffness, 322.27, accuracy: 0.05)
        XCTAssertEqual(FloatMotion.damping, 2 * FloatMotion.stiffness.squareRoot(), accuracy: 0.0001)
    }

    // MARK: 全屏 / 恢复

    func testRotationEnteredThenRestoredStaysPortraitUntilLeaving() {
        var rules = FullscreenRules()
        XCTAssertEqual(rules.request, .system, "turning the phone enters the full screen")
        rules.restore(upright: false)
        XCTAssertEqual(rules.request, .portrait, "no auto full screen after 恢复")
        rules.tilt(upright: true)
        XCTAssertEqual(rules.request, .portrait, "even once the phone is upright")
        rules.left()
        XCTAssertEqual(rules.request, .system, "leaving the page (another tab, background) resets it")
    }

    func testButtonEnteredForcesLandscapeAndRestoreWaitsForUpright() {
        var rules = FullscreenRules()
        rules.enter()
        XCTAssertEqual(rules.request, .landscape)
        rules.restore(upright: false)
        XCTAssertEqual(rules.request, .portrait, "phone still sideways: hold portrait")
        XCTAssertTrue(rules.watchesTilt)
        rules.tilt(upright: false)
        XCTAssertEqual(rules.request, .portrait)
        rules.tilt(upright: true)
        XCTAssertEqual(rules.request, .system, "upright again: rotation works as usual")
        XCTAssertFalse(rules.watchesTilt)
    }

    func testButtonEnteredRestoredWhileUprightRotatesAgainAtOnce() {
        var rules = FullscreenRules()
        rules.enter()
        rules.restore(upright: true)
        XCTAssertEqual(rules.request, .system)
        rules.enter()
        rules.restore(upright: nil)
        XCTAssertEqual(rules.request, .portrait, "unknown tilt (lying flat): hold portrait")
    }

    /// User 2026-11-12: pulling down Control Center / Notification Center or a system alert (the app
    /// only inactive) keeps the full screen; another tab or the background ends it.
    func testOnlyAnotherTabOrTheBackgroundLeavesThePage() {
        XCTAssertFalse(ControlPageExit.left(tabShown: true, phase: .active))
        XCTAssertFalse(ControlPageExit.left(tabShown: true, phase: .inactive), "Control Center is not leaving")
        XCTAssertTrue(ControlPageExit.left(tabShown: true, phase: .background))
        XCTAssertTrue(ControlPageExit.left(tabShown: false, phase: .active), "another tab")
    }

    func testFullScreenSurvivesInactiveButNotBackground() {
        var byButton = FullscreenRules()
        byButton.enter()
        if ControlPageExit.left(tabShown: true, phase: .inactive) { byButton.left() }
        XCTAssertEqual(byButton.request, .landscape, "still landscape with Control Center down")
        if ControlPageExit.left(tabShown: true, phase: .background) { byButton.left() }
        XCTAssertEqual(byButton.request, .system, "the background hands the orientation back")

        var restored = FullscreenRules()
        restored.restore(upright: false)
        if ControlPageExit.left(tabShown: true, phase: .inactive) { restored.left() }
        XCTAssertEqual(restored.request, .portrait, "no auto full screen still holds after inactive")
        if ControlPageExit.left(tabShown: false, phase: .active) { restored.left() }
        XCTAssertEqual(restored.request, .system)
    }

    func testHandleAndExpandedTopInsets() {
        XCTAssertEqual(FloatMotion.handleHeight, 32)
        XCTAssertEqual(FloatMotion.expandedTop, 8)
    }

}
