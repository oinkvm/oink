#if DEBUG
import os
import QuartzCore
import UIKit

/// Scroll frame probe of the transfer-perf measurement (DEBUG, `-oink-perf-out <file>`).
/// While the conversation's scroll view is dragged or decelerating it records every
/// display-link interval and every main run-loop pass (wake-up to sleep, Core Animation
/// commit included), and writes them as JSON when the list comes to rest. Long frames
/// are also os_signpost events (Points of Interest) for Instruments.
/// `-oink-perf-live 1` adds the 1 Hz device-card latency update a connected session
/// delivers (CoreSession `apply(DeviceCardSignals)`), so the page sees the same
/// `CoreSession` changes while scrolling as on a phone with computers online.
@MainActor
final class FrameProbe {
    static let shared = FrameProbe()
    private static let log = OSLog(subsystem: "com.hachiwii.oink", category: .pointsOfInterest)

    private var out: URL?
    private var link: CADisplayLink?
    private var observers: [CFRunLoopObserver] = []
    private weak var scrollView: UIScrollView?
    private var scrolling = false
    private var lastTick: CFTimeInterval?
    private var passStart: CFTimeInterval?
    /// [display-link timestamp, interval ms] while scrolling.
    private var frames: [[Double]] = []
    /// [start, duration ms] of main run-loop passes of 1 ms or more while scrolling.
    private var passes: [[Double]] = []
    private var heartbeat: Timer?

    static func startIfRequested() {
        let args = ProcessInfo.processInfo.arguments
        guard let index = args.firstIndex(of: "-oink-perf-out"), let path = args.dropFirst(index + 1).first else { return }
        shared.start(URL(fileURLWithPath: path), live: args.contains("-oink-perf-live"))
    }

    private func start(_ file: URL, live: Bool) {
        guard out == nil else { return }
        out = file
        let link = CADisplayLink(target: self, selector: #selector(tick(_:)))
        link.add(to: .main, forMode: .common)
        self.link = link
        // First thing after a wake-up, last thing before sleeping (after the CA commit).
        let begin = CFRunLoopObserverCreateWithHandler(nil, CFRunLoopActivity.afterWaiting.rawValue, true, CFIndex.min) { _, _ in
            MainActor.assumeIsolated { FrameProbe.shared.passStart = CACurrentMediaTime() }
        }
        let end = CFRunLoopObserverCreateWithHandler(nil, CFRunLoopActivity.beforeWaiting.rawValue, true, CFIndex.max) { _, _ in
            MainActor.assumeIsolated { FrameProbe.shared.passEnded() }
        }
        for observer in [begin, end].compactMap({ $0 }) {
            CFRunLoopAddObserver(CFRunLoopGetMain(), observer, .commonModes)
            observers.append(observer)
        }
        if live { startHeartbeat() }
    }

    private func passEnded() {
        guard let start = passStart else { return }
        passStart = nil
        let ms = (CACurrentMediaTime() - start) * 1000
        if scrolling, ms >= 1 { passes.append([start, ms]) }
    }

    @objc private func tick(_ link: CADisplayLink) {
        let view = scrollView ?? findScrollView()
        let active = view.map { $0.isTracking || $0.isDragging || $0.isDecelerating } ?? false
        if active, scrolling, let last = lastTick {
            let ms = (link.timestamp - last) * 1000
            frames.append([link.timestamp, ms])
            if ms > 25 { os_signpost(.event, log: Self.log, name: "long frame", "%.1f ms", ms) }
        }
        if scrolling && !active { write() }
        scrolling = active
        lastTick = link.timestamp
    }

    /// The conversation: the tallest scroll view on screen.
    private func findScrollView() -> UIScrollView? {
        let windows = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.flatMap(\.windows)
        var best: UIScrollView?
        func visit(_ view: UIView) {
            if let scroll = view as? UIScrollView, scroll.contentSize.height > (best?.contentSize.height ?? 2000) { best = scroll }
            view.subviews.forEach(visit)
        }
        windows.forEach(visit)
        scrollView = best
        return best
    }

    private func write() {
        guard let out else { return }
        let body: [String: Any] = ["frames": frames, "passes": passes, "screenScale": UIScreen.main.scale,
                                   "maxFPS": UIScreen.main.maximumFramesPerSecond]
        if let data = try? JSONSerialization.data(withJSONObject: body) { try? data.write(to: out) }
    }

    /// What CoreSession `apply(DeviceCardSignals)` does once a second: new latencies
    /// for the online cards.
    private func startHeartbeat() {
        var beat = 0
        heartbeat = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { _ in
            MainActor.assumeIsolated {
                beat += 1
                let core = CoreSession.shared
                guard let cards = core.cards?.cards else { return }
                core.latency.update(cards.enumerated().filter { $0.element.connection != .offline }.map {
                    (device: $0.element.device, ms: 3 + (beat + $0.offset) % 5)
                })
            }
        }
    }
}
#endif
