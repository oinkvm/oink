import Foundation
import UIKit

/// Default name this phone uses in the group (decision 38/39): the system
/// device name when iOS exposes it, otherwise the marketing model name.
///
/// Since iOS 16 `UIDevice.name` returns only the generic model ("iPhone",
/// "iPad") unless the app holds the Apple-approved
/// `com.apple.developer.device-information.user-assigned-device-name`
/// entitlement, which Oink does not request (user decision 2026-11-09). The
/// hardware identifier (`hw.machine`, e.g. "iPhone18,4") is available without
/// any entitlement, so it is mapped to the marketing name ("iPhone Air").
/// Unknown identifiers fall back to `UIDevice.model`. The user can still pick
/// a custom name in Settings (`MobileSession.set_local_device_name`).
enum SystemDeviceName {
    @MainActor static var current: String {
        let device = UIDevice.current
        // With the entitlement (or on an OS that still exposes it) the name is
        // the user-assigned one and differs from the generic model string.
        let name = device.name.trimmingCharacters(in: .whitespacesAndNewlines)
        if !name.isEmpty, name != device.model { return name }
        return marketingName(identifier: hardwareIdentifier) ?? device.model
    }

    static var hardwareIdentifier: String {
        // The simulator reports the host CPU ("arm64") in hw.machine and the
        // simulated model in this environment variable.
        if let simulated = ProcessInfo.processInfo.environment["SIMULATOR_MODEL_IDENTIFIER"] { return simulated }
        var size = 0
        guard sysctlbyname("hw.machine", nil, &size, nil, 0) == 0, size > 0 else { return "" }
        var bytes = [CChar](repeating: 0, count: size)
        guard sysctlbyname("hw.machine", &bytes, &size, nil, 0) == 0 else { return "" }
        return String(cString: bytes)
    }

    static func marketingName(identifier: String) -> String? { models[identifier] }

    /// Devices that run iOS/iPadOS 18 or later. Only identifiers whose
    /// marketing name is certain are listed (checked against the
    /// `modelIdentifier` of Xcode's simulator device types and a paired
    /// iPhone Air reporting iPhone18,4); anything else uses the generic
    /// model name rather than a guess.
    private static let models: [String: String] = {
        var table: [String: String] = [
            "iPhone11,2": "iPhone XS", "iPhone11,4": "iPhone XS Max", "iPhone11,6": "iPhone XS Max",
            "iPhone11,8": "iPhone XR",
            "iPhone12,1": "iPhone 11", "iPhone12,3": "iPhone 11 Pro", "iPhone12,5": "iPhone 11 Pro Max",
            "iPhone12,8": "iPhone SE",
            "iPhone13,1": "iPhone 12 mini", "iPhone13,2": "iPhone 12", "iPhone13,3": "iPhone 12 Pro",
            "iPhone13,4": "iPhone 12 Pro Max",
            "iPhone14,4": "iPhone 13 mini", "iPhone14,5": "iPhone 13", "iPhone14,2": "iPhone 13 Pro",
            "iPhone14,3": "iPhone 13 Pro Max", "iPhone14,6": "iPhone SE",
            "iPhone14,7": "iPhone 14", "iPhone14,8": "iPhone 14 Plus",
            "iPhone15,2": "iPhone 14 Pro", "iPhone15,3": "iPhone 14 Pro Max",
            "iPhone15,4": "iPhone 15", "iPhone15,5": "iPhone 15 Plus",
            "iPhone16,1": "iPhone 15 Pro", "iPhone16,2": "iPhone 15 Pro Max",
            "iPhone17,1": "iPhone 16 Pro", "iPhone17,2": "iPhone 16 Pro Max", "iPhone17,3": "iPhone 16",
            "iPhone17,4": "iPhone 16 Plus", "iPhone17,5": "iPhone 16e",
            "iPhone18,1": "iPhone 17 Pro", "iPhone18,2": "iPhone 17 Pro Max", "iPhone18,3": "iPhone 17",
            "iPhone18,4": "iPhone Air", "iPhone18,5": "iPhone 17e",
            "iPhone19,2": "iPhone 18 Pro", "iPhone19,3": "iPhone 18 Pro Max", "iPhone19,4": "iPhone Duo",
        ]
        func ipad(_ name: String, _ identifiers: String...) {
            for identifier in identifiers { table[identifier] = name }
        }
        ipad("iPad", "iPad7,11", "iPad7,12", "iPad11,6", "iPad11,7", "iPad12,1", "iPad12,2",
             "iPad13,18", "iPad13,19", "iPad15,7", "iPad15,8")
        ipad("iPad mini", "iPad11,1", "iPad11,2", "iPad14,1", "iPad14,2", "iPad16,1", "iPad16,2", "iPad19,2")
        ipad("iPad Air", "iPad11,3", "iPad11,4", "iPad13,1", "iPad13,2", "iPad13,16", "iPad13,17",
             "iPad14,8", "iPad14,9", "iPad14,10", "iPad14,11", "iPad15,3", "iPad15,4", "iPad15,5", "iPad15,6", "iPad16,9", "iPad16,11")
        ipad("iPad Pro", "iPad8,1", "iPad8,2", "iPad8,3", "iPad8,4", "iPad8,5", "iPad8,6", "iPad8,7", "iPad8,8",
             "iPad8,9", "iPad8,10", "iPad8,11", "iPad8,12", "iPad13,4", "iPad13,5", "iPad13,6", "iPad13,7",
             "iPad13,8", "iPad13,9", "iPad13,10", "iPad13,11", "iPad14,3", "iPad14,4", "iPad14,5", "iPad14,6",
             "iPad16,3", "iPad16,4", "iPad16,5", "iPad16,6", "iPad17,2", "iPad17,4")
        return table
    }()
}
