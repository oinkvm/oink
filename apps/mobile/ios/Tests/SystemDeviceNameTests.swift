import XCTest
@testable import OinkAudioTestHost

final class SystemDeviceNameTests: XCTestCase {
    func testKnownIdentifiersMapToMarketingNames() {
        XCTAssertEqual(SystemDeviceName.marketingName(identifier: "iPhone18,4"), "iPhone Air")
        XCTAssertEqual(SystemDeviceName.marketingName(identifier: "iPhone18,3"), "iPhone 17")
        XCTAssertEqual(SystemDeviceName.marketingName(identifier: "iPad14,8"), "iPad Air")
    }

    func testUnknownIdentifierHasNoGuess() {
        XCTAssertNil(SystemDeviceName.marketingName(identifier: "iPhone99,1"))
        XCTAssertNil(SystemDeviceName.marketingName(identifier: "arm64"))
    }

    @MainActor
    func testCurrentNameIsSpecificForAKnownModel() {
        // The simulator exposes its model through SIMULATOR_MODEL_IDENTIFIER;
        // for a known model the default must not be the generic "iPhone".
        let name = SystemDeviceName.current
        XCTAssertFalse(name.isEmpty)
        if SystemDeviceName.marketingName(identifier: SystemDeviceName.hardwareIdentifier) != nil {
            XCTAssertNotEqual(name, UIDevice.current.model)
        }
    }
}
