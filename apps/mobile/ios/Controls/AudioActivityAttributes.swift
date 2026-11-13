import ActivityKit
import Foundation

/// The Live Activity of a running speaker exit or shared microphone (lock
/// screen card and Dynamic Island, mockup B). The app starts one per role
/// when the role is switched on (in the app or from Control Center) and ends
/// it when the role stops; the widget extension draws it.
struct OinkAudioActivityAttributes: ActivityAttributes {
    struct ContentState: Codable, Hashable {
        /// Device names: whose sound plays here (speaker) or who takes the
        /// microphone. Empty while nobody does.
        var peers: [String]
        /// Theme accent, 0xRRGGBB.
        var accent: UInt32
        var forwarding = false
        var paused = false
    }

    var role: AudioActivityText.Role
}
