import XCTest
@testable import OinkAudioTestHost

final class ShareStatusFormatterTests: XCTestCase {
    func testRateUsesThreeSignificantDigitsAndPromotesRoundedUnit() {
        XCTAssertEqual(ShareStatusFormatter.rate(bytesPerSecond: 512), "0.512 KB/s")
        XCTAssertEqual(ShareStatusFormatter.rate(bytesPerSecond: 999_600), "1.00 MB/s")
        XCTAssertEqual(ShareStatusFormatter.rate(bytesPerSecond: 1_230_000_000), "1.23 GB/s")
    }

    func testElapsedBoundaries() {
        XCTAssertEqual(ShareStatusFormatter.elapsed(seconds: 0.420), "0.420")
        XCTAssertEqual(ShareStatusFormatter.elapsed(seconds: 3.18), "3.18")
        XCTAssertEqual(ShareStatusFormatter.elapsed(seconds: 12.5), "12.5")
        XCTAssertEqual(ShareStatusFormatter.elapsed(seconds: 128), "128")
    }
}
