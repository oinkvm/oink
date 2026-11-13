import Foundation

/// Which session the pairing sheet shows. The core's snapshot keeps ended
/// sessions (failed ones and completed ones) next to the running ones, sorted
/// by their random pairing key, so neither the peer device nor the position in
/// the list identifies a new attempt.
enum PairingInbox {
    struct Row: Equatable {
        let id: String
        let incoming: Bool
        let phase: UInt32
        let ended: Bool
        var device = ""
    }

    /// Phase 7 is the core's Completed: the pairing succeeded and stays listed.
    static let completed: UInt32 = 7

    /// Neither failed nor completed.
    static func running(_ row: Row) -> Bool { !row.ended && row.phase < completed }

    /// A tap on a nearby device reopens its running pairing instead of starting another.
    static func runningSession(_ rows: [Row], device: String) -> String? {
        rows.first { $0.device == device && running($0) }?.id
    }

    /// The session the sheet opens after starting or accepting a pairing: the one the core
    /// just began (`started`, the key it returned), never an earlier one.
    static func startedSession(_ rows: [Row], started: String?) -> String? {
        guard let started, rows.contains(where: { $0.id == started }) else { return nil }
        return started
    }
}
