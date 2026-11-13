import XCTest

/// Scroll measurement of the transfer page on the `transfer-perf` fixture (about 420
/// records, DEBUG). The same fixed-speed script every run: 16 flings up into the history,
/// then 16 back down. The app's FrameProbe writes the frame intervals and main run-loop
/// passes of the scrolling to `OINK_PERF_OUT` (`TEST_RUNNER_OINK_PERF_OUT=… xcodebuild
/// test`); `OINK_PERF_LIVE=1` adds the 1 Hz device-card update of a connected session.
/// Without `OINK_PERF_OUT` the test only drives the scrolling.
final class TransferScrollUITests: XCTestCase {
    override func setUp() { continueAfterFailure = false }

    func testScrollConversation() {
        let env = ProcessInfo.processInfo.environment
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", "transfer-perf", "-oink-scheme", "light", "-oink-theme", "pink"]
        if let out = env["OINK_PERF_OUT"] { app.launchArguments += ["-oink-perf-out", out] }
        if env["OINK_PERF_LIVE"] == "1" { app.launchArguments += ["-oink-perf-live", "1"] }
        app.launch()
        let list = app.scrollViews.firstMatch
        XCTAssertTrue(list.waitForExistence(timeout: 30))
        sleep(4)
        let upper = app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.3))
        let lower = app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.7))
        // Before every gesture XCUITest snapshots the app's accessibility tree on the
        // app's main thread (20–40 ms here). Each fling therefore runs out (about 2.4 s
        // from 1200 pt/s) before the next gesture, so the snapshot falls on a list at
        // rest, which the probe does not count.
        let speed = XCUIGestureVelocity(1200)
        func fling(_ from: XCUICoordinate, _ to: XCUICoordinate) {
            from.press(forDuration: 0.02, thenDragTo: to, withVelocity: speed, thenHoldForDuration: 0)
            Thread.sleep(forTimeInterval: 3)
        }
        // The list opens at its end: the finger moves down to go back in the history.
        let count = env["OINK_PERF_FLINGS"].flatMap(Int.init) ?? 16
        for _ in 0..<count { fling(upper, lower) }
        for _ in 0..<count { fling(lower, upper) }
    }
}
