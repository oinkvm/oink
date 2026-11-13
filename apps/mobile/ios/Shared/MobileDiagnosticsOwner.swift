import Foundation

#if OINK_MOBILE_CORE_FFI
/// Owns the mobile diagnostic collector lifetime. The collector is the core's
/// direct NetworkDiagnosticSink; this adapter only supplies allowlisted typed
/// events and never receives or encodes diagnostic wire data.
@MainActor
final class MobileDiagnosticsOwner {
    private var collector: MobileDiagnosticCollector?
    private var flushTask: Task<Void, Never>?

    var isRunning: Bool { collector != nil }

    func start(session: MobileSession) throws {
        guard collector == nil else { return }
        let collector = try session.openDiagnosticsCollector(
            sessionId: UUID().uuidString,
            batchId: UUID().uuidString,
            maxPendingBytes: 256 * 1024,
            segmentBytes: 16 * 1024
        )
        try collector.emit(timestampMs: Self.timestampMilliseconds(), kind: "status",
                           code: "started", metric: nil, count: 1)
        self.collector = collector
        flushTask = Task.detached {
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(10)) } catch { return }
                try? collector.flush()
            }
        }
    }

    func stop() async {
        let flushing = flushTask
        flushTask = nil
        flushing?.cancel()
        guard let collector else { return }
        self.collector = nil
        try? collector.emit(timestampMs: Self.timestampMilliseconds(), kind: "status",
                            code: "stopped", metric: nil, count: 1)
        await flushing?.value
        _ = await Task.detached { try? collector.flush() }.value
    }

    func emit(kind: String, code: String, metric: UInt64? = nil, count: UInt32 = 1) {
        guard let collector else { return }
        try? collector.emit(timestampMs: Self.timestampMilliseconds(), kind: kind,
                            code: code, metric: metric, count: count)
    }

    func setPeers(_ peers: [MobilePeer]) {
        _ = try? collector?.setPeers(peers: peers)
    }

    private static func timestampMilliseconds() -> UInt64 {
        UInt64(max(0, Date().timeIntervalSince1970 * 1000))
    }
}
#endif
