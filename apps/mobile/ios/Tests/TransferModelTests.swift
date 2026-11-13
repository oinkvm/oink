import Combine
import SwiftUI
import XCTest
@testable import OinkAudioTestHost

/// The session side of the transfer history, shaped like CoreSession: record changes
/// and progress are separate published values, changed together in one main-queue
/// turn when a transfer ends (DeliveryEvent / DirectEnded clear the progress).
@MainActor
private final class FakeHistory: ObservableObject, TransferHistorySource {
    @Published var transferChange = IOSTransferChange()
    @Published var sessionRevision = 0
    @Published var progress: [Int64: Int] = [:]
    var records: [Int64: TransferMessage] = [:]
    var recordProgress: [Int64: TransferOperationProgress] = [:]
    var moreShared = false
    var pageFails = false
    func sharedHistoryHasMore() -> Bool { moreShared }
    func loadOlderSharedHistory() async throws {
        if pageFails { throw NSError(domain: "history-test", code: 1) }
        moreShared = false
    }

    var transferChanges: AnyPublisher<IOSTransferChange, Never> { $transferChange.eraseToAnyPublisher() }
    var sessionRevisions: AnyPublisher<Int, Never> { $sessionRevision.eraseToAnyPublisher() }
    var hasLiveSession: Bool { true }

    func transferPage(createdMs: Int64?, id: Int64?, limit: UInt32) throws -> CoreTransferPage {
        CoreTransferPage(messages: records.values.sorted { $0.createdAt > $1.createdAt }, nextCreatedMs: nil, nextID: nil)
    }
    func transferMessage(_ id: Int64) throws -> TransferMessage? { records[id] }
    func deleteTransferMessage(_ message: TransferMessage) throws {}
    func requestTransferThumbnail(_ message: TransferMessage) {}
    func startTransferDownload(_ message: TransferMessage, open: Bool) throws {}

    func changed(_ ids: [Int64]) {
        transferChange = IOSTransferChange(sequence: transferChange.sequence &+ 1, recordIDs: ids)
    }

    static func image(_ id: Int64, mine: Bool, state: AttachmentState?, delivery: TransferDeliveryState?) -> TransferMessage {
        TransferMessage(id: String(id), createdAt: Date(timeIntervalSince1970: 1_800_000_000 + Double(id)),
                        senderDeviceID: mine ? "self" : "phone", senderName: "Phone", offerID: nil, sourceBoot: nil,
                        operationID: "op-\(id)", itemID: 0, representationID: 1, originSenderID: nil, proxyDeviceID: nil,
                        proxyName: nil, kind: .image, text: nil, contentID: "content-\(id)", attachmentName: "IMG_\(id).jpg",
                        byteSize: 3_000_000, uti: "image/jpeg", state: state, availability: "Ready", isMine: mine,
                        delivery: delivery, directPeerID: "phone", directPeerName: "Phone")
    }
}

/// Stands in for the transfer page: it owns the model and redraws on the progress
/// it shows, like TransferView with TransferPageSource.percent.
private struct Page: View {
    @StateObject var model: TransferModel
    @ObservedObject var history: FakeHistory
    var body: some View {
        VStack {
            ForEach(model.messages) { message in
                Text("\(message.id) \(String(describing: message.state)) \(String(describing: message.delivery))")
            }
            Text("\(history.progress.count)")
        }
    }
}

@MainActor
final class TransferModelTests: XCTestCase {
    private func settle(until done: () -> Bool) {
        let deadline = Date().addingTimeInterval(2)
        while !done(), Date() < deadline { RunLoop.main.run(until: Date().addingTimeInterval(0.02)) }
    }

    /// Bug A: an image this phone sent to another phone stayed「发送中」until restart.
    /// The DeliveryEvent reloads the record; the model must apply it on its own.
    func testOwnDirectShareShowsItsDeliveryWhenItSettles() {
        let history = FakeHistory()
        history.records[1] = FakeHistory.image(1, mine: true, state: .stored, delivery: .sending)
        let model = TransferModel(core: history)
        model.loadInitial()
        settle { !model.messages.isEmpty }
        XCTAssertEqual(model.messages.first?.delivery, .sending)

        // One listener batch: the DeliveryEvent clears the record's progress and
        // reports the record, whose stored delivery is now Sent.
        DispatchQueue.main.async {
            history.records[1]?.delivery = .sent
            history.progress[1] = nil
            history.changed([1])
        }
        settle { model.messages.first?.delivery == .sent }
        XCTAssertEqual(model.messages.first?.delivery, .sent)
    }

    /// Bug B: a phone-to-phone receive stayed at 0 % until restart while the page was
    /// on screen. The last progress, the completed record and DirectEnded arrive in one
    /// batch together with progress changes that redraw the page.
    func testDirectReceiveCompletesWhileThePageRedraws() {
        let history = FakeHistory()
        history.records[1] = FakeHistory.image(1, mine: false, state: .stored, delivery: nil)
        let model = TransferModel(core: history)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UIHostingController(rootView: Page(model: model, history: history))
        window.makeKeyAndVisible()
        defer { window.isHidden = true }
        model.loadInitial()
        RunLoop.main.run(until: Date().addingTimeInterval(0.3))  // the page draws once

        for id: Int64 in 2...6 {
            // DirectIncoming: the first record exists and is downloading.
            DispatchQueue.main.async {
                history.records[id] = FakeHistory.image(id, mine: false, state: .downloading, delivery: nil)
                history.changed([id])
            }
            settle { model.messages.contains { $0.id == String(id) } }
            // finish_item + DirectEnded: progress to 100 %, record complete, progress cleared.
            DispatchQueue.main.async {
                history.progress[id] = 100
                history.records[id]?.state = .stored
                history.changed([id])
                history.progress[id] = nil
                history.changed([id])
            }
            settle { model.messages.first { $0.id == String(id) }?.state == .stored }
            XCTAssertEqual(model.messages.first { $0.id == String(id) }?.state, .stored, "record \(id)")
        }
    }

    func testInitialSharedPageFailureOffersRetryBeforeReadingTheLocalTail() {
        let history = FakeHistory()
        history.moreShared = true
        history.pageFails = true
        let model = TransferModel(core: history)
        model.loadInitial()
        settle { model.olderLoadFailed }
        XCTAssertTrue(model.olderLoadFailed)
        XCTAssertTrue(model.messages.isEmpty)
        history.pageFails = false
        history.records[1] = FakeHistory.image(1, mine: false, state: .stored, delivery: nil)
        model.loadMore()
        settle { !model.isLoading }
        XCTAssertFalse(model.olderLoadFailed)
        XCTAssertEqual(model.messages.map(\.id), ["1"])
    }

    func testFailedAutomaticHistoryLoadCanBeRetried() {
        let history = FakeHistory()
        history.records[2] = FakeHistory.image(2, mine: false, state: .stored, delivery: nil)
        let model = TransferModel(core: history)
        model.loadInitial()
        settle { !model.messages.isEmpty }
        history.moreShared = true
        history.pageFails = true
        model.loadMore()
        settle { model.olderLoadFailed }
        XCTAssertTrue(model.olderLoadFailed)
        history.pageFails = false
        model.loadMore()
        settle { !model.isLoading }
        XCTAssertFalse(model.olderLoadFailed)
    }

    func testSupersededPasswordIsRemovedFromTheVisibleList() {
        let history = FakeHistory()
        var password = FakeHistory.image(1, mine: false, state: .stored, delivery: nil)
        password.sensitive = true
        history.records[1] = password
        let model = TransferModel(core: history)
        model.loadInitial()
        settle { !model.messages.isEmpty }
        history.records[1] = nil
        history.changed([1])
        settle { model.messages.isEmpty }
        XCTAssertTrue(model.messages.isEmpty)
    }

    /// A restarted session reloads the history.
    func testNewSessionReloadsTheHistory() {
        let history = FakeHistory()
        let model = TransferModel(core: history)
        model.loadInitial()
        XCTAssertTrue(model.messages.isEmpty)
        history.records[1] = FakeHistory.image(1, mine: false, state: .stored, delivery: nil)
        history.sessionRevision += 1
        settle { !model.messages.isEmpty }
        XCTAssertEqual(model.messages.map(\.id), ["1"])
    }
}
