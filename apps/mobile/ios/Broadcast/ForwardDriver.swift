import Foundation
import CoreMedia

/// One serialized non-RT worker owns the forward handle, including every push,
/// revoke and destroy. The app drains the ring here; ReplayKit uses `capture`.
final class ForwardDriver: @unchecked Sendable {
    private let queue = DispatchQueue(label: "com.hachiwii.oink.forward")
    private var media: MobileMediaSession?
    private var subscription: MobileMediaSubscription?
    private var handle: OpaquePointer?
    private var generation: UInt64 = 0
    private var rate: UInt32 = 0
    private var channels: UInt16 = 0
    private var lastHost: UInt64 = 0
    private var discontinuous = true
    private var paused = false
    private var timer: DispatchSourceTimer?
    private let converter = OinkReplayPCM()
    let pipe: OinkBroadcastPipe
    var didDisable: (@Sendable () -> Void)?
    var invalidFormat: (@Sendable () -> Void)?

    init(pipe: OinkBroadcastPipe) { self.pipe = pipe }

    func bind(_ media: MobileMediaSession?, readRing: Bool = false) {
        queue.sync {
            timer?.cancel(); timer = nil
            subscription?.cancel(); subscription = nil
            stopHandle()
            self.media = media
            rate = 0; channels = 0
            guard let media, !pipe.stopRequested else { return }
            _ = media.setEnabled(kind: .forward, enabled: true)
            subscription = media.subscribe(listener: ForwardMediaEvents { [weak self] in
                guard let self else { return }
                self.queue.async { [weak self] in self?.reconcile() }
            })
            if readRing {
                let timer = DispatchSource.makeTimerSource(queue: queue)
                timer.schedule(deadline: .now(), repeating: .milliseconds(5))
                timer.setEventHandler { [weak self] in
                    guard let self else { return }
                    self.pipe.drain { pcm, frames, rate, channels, host, gap in
                        self.push(pcm, frames: frames, rate: rate, channels: channels, host: host, gap: gap)
                    }
                }
                self.timer = timer
                timer.resume()
            }
            reconcile()
        }
    }

    /// Video and mic buffers never enter this worker and are never retained.
    func capture(_ sample: CMSampleBuffer) {
        queue.sync {
            guard !pipe.paused, !pipe.stopRequested else { return }
            let valid = converter.convert(sample) { pcm, frames, rate, channels, host, gap in
                if self.media != nil {
                    self.push(pcm, frames: frames, rate: rate, channels: channels, host: host, gap: gap)
                } else {
                    self.pipe.writePCM(pcm, frames: frames, rate: rate, channels: channels,
                        hostNS: host, discontinuity: gap)
                }
            }
            if !valid { invalidFormat?() }
        }
    }

    func setPaused(_ paused: Bool) {
        queue.sync {
            guard self.paused != paused else { return }
            self.paused = paused
            if paused {
                // A snapshot callback may already have retired the handle on
                // seeing the shared pause flag. Advance the epoch even then.
                if let media { _ = media.requestRebuildResources(resources: 4) }
                stopHandle()
            } else {
                if let media { _ = media.clearFailure(kind: .forward) }
                discontinuous = true
                reconcile()
            }
        }
    }

    private func push(_ pcm: UnsafePointer<Float>, frames: UInt32, rate: UInt32,
                      channels: UInt16, host: UInt64, gap: Bool) {
        guard !pipe.paused, !pipe.stopRequested else { discontinuous = true; return }
        if self.rate != rate || self.channels != channels {
            if let media, handle != nil { _ = media.requestRebuildResources(resources: 4) }
            stopHandle()
            self.rate = rate; self.channels = channels
            reconcile()
        }
        reconcile()
        guard let handle else { discontinuous = true; return }
        // ReplayKit timestamp fits may drift. Spacing alone is not proof of
        // missing PCM; report known ring drops and actual clock resets only.
        let reset = host != 0 && lastHost != 0 && host < lastHost
        let accepted = oink_audio_forward_push(handle, pcm, frames, host, (gap || discontinuous || reset) ? 1 : 0)
        discontinuous = accepted != frames
        lastHost = host
    }

    private func reconcile() {
        guard let media else { return }
        var state = media.snapshot()
        if handle != nil && (state.stopMask & 4 != 0 || state.ownerGenerations[2] != generation || pipe.paused || pipe.stopRequested) {
            stopHandle()
            state = media.snapshot()
        }
        guard state.localEnabledMask & 4 != 0 else { didDisable?(); return }
        guard !pipe.paused, !pipe.stopRequested, handle == nil, rate != 0, state.startMask & 4 != 0,
              media.claimStart(kind: .forward, generation: state.generation) else { return }
        generation = state.generation
        guard let created = oink_audio_forward_create(rate, channels, generation) else {
            _ = media.reportFailure(kind: .forward, generation: generation, failure: .unavailable)
            _ = media.acknowledgeStopped(kind: .forward, generation: generation)
            generation = 0
            return
        }
        handle = created
        guard media.attachAudio(kind: .forward, generation: generation, handle: UInt64(UInt(bitPattern: created))),
              media.acknowledgeStarted(kind: .forward, generation: generation) else {
            stopHandle(); return
        }
        discontinuous = true
    }

    private func stopHandle() {
        if let handle {
            oink_audio_forward_revoke(handle)
            oink_audio_forward_destroy(handle)
        }
        handle = nil
        if let media, generation != 0 { _ = media.acknowledgeStopped(kind: .forward, generation: generation) }
        generation = 0; lastHost = 0; discontinuous = true
    }
}
private final class ForwardMediaEvents: MobileMediaListener, @unchecked Sendable {
    private let changed: @Sendable () -> Void
    init(_ changed: @escaping @Sendable () -> Void) { self.changed = changed }
    func onMediaState(state: MobileMediaState) { changed() }
}
