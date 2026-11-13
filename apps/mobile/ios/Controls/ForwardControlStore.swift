import CoreFoundation
import Foundation
import WidgetKit

/// Controls read ReplayKit's authoritative kernel-backed lifetime even after
/// the app releases its session and the broadcast extension becomes owner.
struct ForwardControlsState: Codable, Equatable {
    var active: Bool
    var paused: Bool
    var exit: String?
}

enum ForwardControlStore {
    static let stateName = "forward-controls.json"
    static func read(in directory: URL) -> ForwardControlsState? {
        guard let data = try? Data(contentsOf: directory.appendingPathComponent(stateName)) else { return nil }
        return try? JSONDecoder().decode(ForwardControlsState.self, from: data)
    }
    static func write(_ state: ForwardControlsState, in directory: URL) throws {
        try JSONEncoder().encode(state).write(to: directory.appendingPathComponent(stateName),
            options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
    }

    static func active(in directory: URL) -> Bool {
        OinkBroadcastPipe(root: directory.path)?.broadcasting == true
    }

    static func stop() {
        guard let directory = ControlsStore.directory(), let pipe = OinkBroadcastPipe(root: directory.path),
              pipe.broadcasting else { return }
        pipe.stopRequested = true
        let group = Bundle.main.object(forInfoDictionaryKey: "OinkAppGroup") as? String ?? ""
        CFNotificationCenterPostNotification(CFNotificationCenterGetDarwinNotifyCenter(),
            CFNotificationName((group + ".broadcast.changed") as CFString), nil, nil, true)
        reload()
    }

    static func reload() {
        for kind in ControlKind.all { ControlCenter.shared.reloadControls(ofKind: kind) }
    }
}
