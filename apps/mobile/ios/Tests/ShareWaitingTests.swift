import XCTest
@testable import OinkAudioTestHost

/// The share extension sending to the group's only other device while it is offline
/// (user 2026-11-11): the card says so with 取消 only, and turns into the progress card
/// as soon as the core starts the share.
final class ShareWaitingTests: XCTestCase {
    private let sendingText = "正在发送给 Android…"

    func testWaitingTextHasNoTrailingPeriod() {
        XCTAssertEqual(ShareUiState.waitingText, AppText.pick("对方不在线，请在对方设备打开 Oink\n并确保网络连接畅通",
            "The other device is offline. Open Oink on it and make sure it's connected to the network"))
        XCTAssertFalse(ShareUiState.waitingText.hasSuffix("。"))
        XCTAssertFalse(ShareUiState.waitingText.hasSuffix("."))
    }

    func testSendingShareWaitsThenStartsWithoutATap() {
        let sending = ShareUiState(phase: .sending, statusText: sendingText, progress: nil, toPhone: true)
        let waiting = sending.targetWaiting(true, sendingText: sendingText)
        XCTAssertEqual(waiting, ShareUiState(phase: .waiting, statusText: ShareUiState.waitingText, progress: nil, toPhone: true))
        // Repeated reports keep it waiting.
        XCTAssertEqual(waiting.targetWaiting(true, sendingText: sendingText), waiting)
        // Online: back to the progress card, progress unknown until the first sample.
        XCTAssertEqual(waiting.targetWaiting(false, sendingText: sendingText),
                       ShareUiState(phase: .sending, statusText: sendingText, progress: nil, toPhone: true))
    }

    func testOnlyASendingShareCanWait() {
        let progress = ShareUiState(phase: .sending, statusText: sendingText, progress: 0.4, toPhone: false)
        // Not waiting: a sending card keeps its progress.
        XCTAssertEqual(progress.targetWaiting(false, sendingText: sendingText), progress)
        for phase in [ShareUiState.Phase.preparing, .failed, .succeeded] {
            let state = ShareUiState(phase: phase, statusText: "x", progress: nil)
            XCTAssertEqual(state.targetWaiting(true, sendingText: sendingText), state)
            XCTAssertEqual(state.targetWaiting(false, sendingText: sendingText), state)
        }
    }
}
