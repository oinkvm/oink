import XCTest
@testable import OinkAudioTestHost

/// M18: the host shows the typed reason the core reports, never a generic
/// "connection lost" for a source that is offline, evicted or replaced.
final class TransferFailureTextTests: XCTestCase {
    func testSourceReasonsStayDistinct() {
        let texts = [TransferFailure.sourceOffline, .evicted, .noSuchItem, .superseded, .expired, .noTarget, .linkLost]
            .map(TransferFailureText.text)
        XCTAssertEqual(Set(texts).count, texts.count)
        XCTAssertEqual(TransferFailureText.text(.noTarget), "没有在线的电脑。")
    }

    func testAFailedFolderListingSaysGoneOrWhyItCouldNotBeRead() {
        // Gone for good: one text, not the download wording.
        XCTAssertEqual(TransferFailureText.folder(.evicted), TransferFailureText.folder(.noSuchItem))
        XCTAssertNotEqual(TransferFailureText.folder(.evicted), TransferFailureText.text(.evicted))
        // Otherwise: why it could not be read now.
        XCTAssertTrue(TransferFailureText.folder(.linkLost).hasSuffix(TransferFailureText.text(.linkLost)))
        XCTAssertTrue(TransferFailureText.folder(.sourceOffline).hasSuffix(TransferFailureText.text(.sourceOffline)))
    }

    func testStoredAvailabilityExplainsAnUnavailableRecord() {
        XCTAssertNil(TransferFailureText.availability("Ready"))
        XCTAssertEqual(TransferFailureText.availability("Superseded"), TransferFailureText.text(.superseded))
        XCTAssertEqual(TransferFailureText.availability("NoSuchItem"), TransferFailureText.text(.noSuchItem))
        XCTAssertEqual(TransferFailureText.availability("SourceOffline"), TransferFailureText.text(.sourceOffline))
    }
}
