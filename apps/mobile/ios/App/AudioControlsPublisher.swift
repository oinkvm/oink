import ActivityKit
import Combine
import Foundation
import os
import WidgetKit

/// Publishes this phone's speaker / microphone switches outside the app
/// (user 2026-11-12): the App Group state the Control Center toggles read
/// (`ControlsState`) and one Live Activity per running role. Both follow the
/// switch while the app's session lives. Without a session nothing runs (the
/// accepted switch only waits in memory for the next one), so the toggle shows
/// off; the Live Activity ends too, unless an OS interruption released the
/// session (AudioActivityRule): then it stays and the resumed audio keeps it.
@MainActor
final class AudioControlsPublisher {
    nonisolated private static let log = Logger(subsystem: Bundle.main.bundleIdentifier ?? "com.hachiwii.oink", category: "controls")
    private let liveness = ControlsLiveness()
    private var live = false
    /// The session ended because an OS interruption stopped the audio (AudioActivityRule).
    private var interruptionRelease = false
    private var written: ControlsState?
    private var activities: [AudioActivityText.Role: Activity<OinkAudioActivityAttributes>] = [:]
    /// Live Activity requests (end / update) still running. ActivityKit only
    /// acts once `end` has run; an app that iOS suspends before then keeps the
    /// card on the Lock Screen and in the Dynamic Island for hours (user
    /// 2026-11-12: the PCs listed the iPhone offline, the speaker card stayed).
    /// The background release awaits them (`settle`) before it lets go of its
    /// background task.
    private var pending: [UUID: Task<Void, Never>] = [:]
    private var observation: AnyCancellable?
    private var forwardObservation: AnyCancellable?
    private weak var core: CoreSession?

    init(core: CoreSession) {
        self.core = core
        // Live Activities of an earlier process (killed with a switch on)
        // stay until ended; this process starts its own when a role runs.
        for activity in Activity<OinkAudioActivityAttributes>.activities {
            track { await activity.end(nil, dismissalPolicy: .immediate) }
        }
        forwardObservation = core.forwarding.$state.sink { [weak self] _ in
            // @Published emits before the property changes. Re-read after it commits.
            Task { @MainActor in self?.refresh() }
        }
        observation = core.$localAudio.combineLatest(core.$cards)
            .sink { [weak self] audio, cards in self?.refresh(audio: audio, cards: cards?.cards ?? []) }
    }

    /// The app's network session started or ended (CoreSession); `interruption`: it ended because
    /// an OS interruption stopped the audio, and the Live Activities stay (AudioActivityRule).
    /// Ending it returns once the Live Activities have ended or been updated: the caller runs on
    /// a background task that may be the process's last execution time.
    func sessionChanged(live: Bool, interruption: Bool = false) async {
        if live, let directory = ControlsStore.directory() { liveness.hold(in: directory) }
        if !live { liveness.release() }
        self.live = live && liveness.isHolding
        interruptionRelease = !live && interruption
        refresh()
        await settle()
    }

    /// The process is about to exit (`applicationWillTerminate`): ends every
    /// audio Live Activity before returning. Live Activities outlive their
    /// process; when the user swiped Oink away in the app switcher while the
    /// speaker played (user 2026-11-12, iPhone log: runningboardd
    /// "Graceful Termination: user initiated quit", exit 0xdeadfa11, the
    /// process gone 50 ms later), the card stayed on the Lock Screen for
    /// hours with nothing left to end it. UIKit exits as soon as this
    /// returns and the main thread is blocked meanwhile, so the ends run off
    /// the main actor and this waits for them, bounded well inside the
    /// system's 6 s termination grace.
    nonisolated static func endActivitiesBeforeExit() {
        let running = Activity<OinkAudioActivityAttributes>.activities
        log.info("terminating: ending \(running.count, privacy: .public) live activities")
        guard !running.isEmpty else { return }
        let done = DispatchSemaphore(value: 0)
        Task.detached {
            for activity in running { await activity.end(nil, dismissalPolicy: .immediate) }
            done.signal()
        }
        if done.wait(timeout: .now() + 3) == .timedOut {
            log.error("terminating: live activities not ended within 3 s")
        }
    }

    /// Waits for the Live Activity requests made so far (ends and updates).
    func settle() async {
        while let task = pending.values.first {
            await task.value
        }
    }

    /// Runs one ActivityKit request, tracked until it returns (`settle`).
    private func track(_ work: @escaping @MainActor () async -> Void) {
        let id = UUID()
        pending[id] = Task { @MainActor [weak self] in
            await work()
            self?.pending[id] = nil
        }
    }

    /// Re-reads the switches, the device names and the theme accent (also
    /// called when the accent changes) and publishes what changed.
    func refresh() {
        guard let core else { return }
        refresh(audio: core.localAudio, cards: core.cards?.cards ?? [])
    }

    private func refresh(audio: IOSLocalAudio, cards: [IOSDeviceCard]) {
        let accent = (OinkPalette.Accent(rawValue: OinkPrefs.shared.string(forKey: OinkPrefs.accent) ?? "") ?? .pink).base
        let forwarding = core?.forwarding.state != .stopped
        let paused = core?.forwarding.state == .paused
        let state = ControlsState(speaker: live && audio.speakerOn && !forwarding,
            microphone: live && audio.microphoneOn, forwarding: forwarding,
            forwardingPaused: paused, forwardExit: forwarding ? cards.first { !$0.isSelf && $0.controls.speaker == .outlet }?.name : nil,
            accent: accent)
        if state != written, let directory = ControlsStore.directory() {
            do {
                try ControlsStore.write(state, in: directory)
                written = state
                for kind in ControlKind.all { ControlCenter.shared.reloadControls(ofKind: kind) }
            } catch {
                Self.log.error("controls state not written: \(error.localizedDescription, privacy: .public)")
            }
        }
        let shows = { (on: Bool) in
            AudioActivityRule.shows(switchOn: on, sessionLive: self.live, interruptionRelease: self.interruptionRelease)
        }
        let exits = cards.filter { !$0.isSelf && $0.connection != .offline && $0.controls.speaker == .outlet }.map(\.name)
        sync(.speaker, on: forwarding || shows(audio.speakerOn), peers: forwarding ? exits : Self.peers(.speaker, cards),
             accent: accent, forwarding: forwarding, paused: paused)
        sync(.microphone, on: shows(audio.microphoneOn), peers: Self.peers(.microphone, cards), accent: accent)
    }

    /// Starts, updates or ends the role's Live Activity. A Control Center
    /// intent publishes from its perform (`AudioControlActions`), which is
    /// what lets a LiveActivityIntent start it in the background; the
    /// microphone's AudioRecordingIntent needs it while it records.
    private func sync(_ role: AudioActivityText.Role, on: Bool, peers: [String], accent: UInt32, forwarding: Bool = false, paused: Bool = false) {
        guard on else {
            if let activity = activities.removeValue(forKey: role) {
                track { await activity.end(nil, dismissalPolicy: .immediate) }
            }
            return
        }
        let content = OinkAudioActivityAttributes.ContentState(peers: peers, accent: accent, forwarding: forwarding, paused: paused)
        // Apple documents local ActivityKit mutations from the containing app.
        // Once the app yields to an upload extension, it may be suspended. Mark
        // its last card stale at that handoff rather than assert a live status
        // that this process can no longer keep current. Reconcile on next run.
        let staleDate: Date? = forwarding && !live ? .distantPast : nil
        if let activity = activities[role] {
            guard activity.content.state != content || activity.content.staleDate != staleDate else { return }
            track { await activity.update(ActivityContent(state: content, staleDate: staleDate)) }
            return
        }
        do {
            activities[role] = try Activity.request(attributes: OinkAudioActivityAttributes(role: role),
                                                    content: ActivityContent(state: content, staleDate: staleDate),
                                                    pushType: nil)
        } catch {
            // Live Activities switched off in Settings, or a background start
            // outside a LiveActivityIntent: the role runs without the card.
            Self.log.error("\(role.rawValue, privacy: .public) live activity not started: \(error.localizedDescription, privacy: .public)")
        }
    }

    /// Whose sound this phone plays (devices forwarding to the exit) or who
    /// takes its microphone, by the device cards the list shows.
    static func peers(_ role: AudioActivityText.Role, _ cards: [IOSDeviceCard]) -> [String] {
        cards.filter { card in
            guard !card.isSelf, card.connection != .offline else { return false }
            switch role {
            case .speaker: return card.controls.speaker == .forward
            case .microphone: return card.controls.mic == .receive
            }
        }.map(\.name)
    }
}
