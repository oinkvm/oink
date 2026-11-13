import Foundation
import CoreFoundation

/// Cross-process hint that an Oink process released the core's owner lock
/// (review M27). The share extension posts it after its session shut down;
/// a main app whose start failed with `OwnerBusy` starts its session then,
/// without polling the lock. A Darwin notification carries no data and is
/// not queued for a suspended process: it is a hint for a running app; the
/// app also retries whenever it becomes active and from the retry button.
/// An extension killed without shutting down posts nothing; the kernel
/// still frees its lock, so those later retries succeed.
final class OwnerReleaseSignal {
    private static var name: String { SharedStore.group + ".owner-released" }
    private var handler: (@MainActor () -> Void)?

    static func post() {
        CFNotificationCenterPostNotification(CFNotificationCenterGetDarwinNotifyCenter(),
            CFNotificationName(name as CFString), nil, nil, true)
    }

    deinit {
        CFNotificationCenterRemoveObserver(CFNotificationCenterGetDarwinNotifyCenter(),
            Unmanaged.passUnretained(self).toOpaque(), CFNotificationName(Self.name as CFString), nil)
    }

    /// Observe while this owner lives (the broadcast can end without process exit).
    func observe(_ handler: @escaping @MainActor () -> Void) {
        guard self.handler == nil else { return }
        self.handler = handler
        CFNotificationCenterAddObserver(CFNotificationCenterGetDarwinNotifyCenter(),
            Unmanaged.passUnretained(self).toOpaque(), { _, pointer, _, _, _ in
                guard let pointer else { return }
                let instance = Unmanaged<OwnerReleaseSignal>.fromOpaque(pointer).takeUnretainedValue()
                Task { @MainActor in instance.handler?() }
            }, Self.name as CFString, nil, .deliverImmediately)
    }
}
