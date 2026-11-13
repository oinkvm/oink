import Foundation
import CoreFoundation

/// Signals are hints. File locks and mmap flags are authoritative, including
/// after either process is killed (Darwin notifications are not queued).
final class BroadcastSignal {
    enum Event: String, CaseIterable { case changed, appRequested }
    private let event: Event
    private let handler: @MainActor () -> Void
    init(_ event: Event, handler: @escaping @MainActor () -> Void) {
        self.event = event; self.handler = handler
        CFNotificationCenterAddObserver(CFNotificationCenterGetDarwinNotifyCenter(),
            Unmanaged.passUnretained(self).toOpaque(), { _, pointer, _, _, _ in
                guard let pointer else { return }
                let signal = Unmanaged<BroadcastSignal>.fromOpaque(pointer).takeUnretainedValue()
                let handler = signal.handler
                Task { @MainActor in handler() }
            }, Self.name(event), nil, .deliverImmediately)
    }
    static func post(_ event: Event) {
        CFNotificationCenterPostNotification(CFNotificationCenterGetDarwinNotifyCenter(),
            CFNotificationName(name(event)), nil, nil, true)
    }
    private static func name(_ event: Event) -> CFString { (SharedStore.group + ".broadcast." + event.rawValue) as CFString }
    deinit {
        CFNotificationCenterRemoveObserver(CFNotificationCenterGetDarwinNotifyCenter(),
            Unmanaged.passUnretained(self).toOpaque(), CFNotificationName(Self.name(event)), nil)
    }
}
