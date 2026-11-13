import Foundation
import WidgetKit

/// UI publication only. The pipe and network session remain authoritative.
/// ActivityKit's documented local update/end path is the containing app, so
/// this extension publishes controls and leaves its app-owned activity stale
/// until the main app reconciles it (AudioControlsPublisher).
@MainActor
final class BroadcastUIPublisher {
    private var written: ForwardControlsState?
    func publish(active: Bool, paused: Bool, exit: String? = nil) {
        guard let directory = ControlsStore.directory() else { return }
        let state = ForwardControlsState(active: active, paused: active && paused, exit: active ? exit : nil)
        guard state != written else { return }
        do {
            try ForwardControlStore.write(state, in: directory)
            written = state
            ForwardControlStore.reload()
        } catch {
            // A subsequent real state change or core snapshot retries publication.
            NSLog("Oink broadcast controls publication failed: %@", error.localizedDescription)
        }
    }
}
