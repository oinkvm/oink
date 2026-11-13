import AVFAudio
import XCTest
import UIKit
@testable import OinkAudioTestHost

final class AudioLifecycleTests: XCTestCase {
    @MainActor private func withAudio(_ body: (MobileAudioSession) async throws -> Void) async rethrows {
        AudioSystemStub.install()
        var generation: UInt64 = 0
        let audio = MobileAudioSession(backend: OinkAudioBackend(), prepareGeneration: { _, _ in
            generation += 1
            return generation
        }, stoppedGeneration: { _ in })
        defer {
            audio.stopFromPhone()
            AudioSystemStub.uninstall()
        }
        try await body(audio)
    }

    @MainActor private func postInterruption(_ type: AVAudioSession.InterruptionType,
                                            extra: [String: Any] = [:]) {
        var info = extra
        info[AVAudioSessionInterruptionTypeKey] = NSNumber(value: type.rawValue)
        NotificationCenter.default.post(name: AVAudioSession.interruptionNotification, object: nil, userInfo: info)
    }

    @MainActor func testForwardingMixPolicyDoesNotCreateBackgroundPlayback() async {
        await withAudio { audio in
            audio.setForwardingCaptureActive(true)
            await audio.waitForTransition()
            XCTAssertEqual(AudioSystemStub.activationCount(), 0)
            XCTAssertFalse(audio.keepsBackgroundSession)

            await audio.setLocalIntent(playback: false, microphone: true)
            await audio.waitForTransition()
            XCTAssertTrue(AVAudioSession.CategoryOptions(rawValue: AudioSystemStub.lastCategoryOptions()).contains(.mixWithOthers))

            audio.setForwardingCaptureActive(false)
            await audio.waitForTransition()
            XCTAssertFalse(AVAudioSession.CategoryOptions(rawValue: AudioSystemStub.lastCategoryOptions()).contains(.mixWithOthers))
            XCTAssertEqual(AudioSystemStub.deactivationCount(), 0)
        }
    }

    @MainActor func testPhoneResumeWithoutInterruptionEnded() async {
        await withAudio { audio in
            await audio.setLocalIntent(playback: true, microphone: false)
            await audio.waitForTransition()
            postInterruption(.began)
            XCTAssertEqual(audio.state, .interrupted)
            XCTAssertFalse(AudioSystemStub.isRunning())

            await audio.setLocalIntent(playback: true, microphone: false)
            await audio.waitForTransition()
            XCTAssertEqual(audio.state, .running)
            XCTAssertFalse(audio.requiresExplicitLocalResume)
            XCTAssertEqual(AudioSystemStub.activationCount(), 2)
            XCTAssertEqual(AudioSystemStub.deactivationCount(), 0)

            // A late ended event from the old interruption cannot stop this run.
            postInterruption(.ended)
            XCTAssertEqual(audio.state, .running)
        }
    }

    @MainActor func testPhoneStopClearsMissingInterruptionEnd() async {
        await withAudio { audio in
            await audio.setLocalIntent(playback: true, microphone: false)
            await audio.waitForTransition()
            postInterruption(.began)
            audio.stopFromPhone()
            XCTAssertEqual(audio.state, .stopped)
            XCTAssertFalse(audio.requiresExplicitLocalResume)
            postInterruption(.ended)
            XCTAssertEqual(audio.state, .stopped)
            await audio.setLocalIntent(playback: true, microphone: false)
            await audio.waitForTransition()
            XCTAssertEqual(audio.state, .running)
        }
    }

    @MainActor func testDelayedSuspensionBeginDoesNotInterruptRecoveredAudio() async {
        await withAudio { audio in
            await audio.setLocalIntent(playback: true, microphone: false)
            await audio.waitForTransition()
            AudioSystemStub.postLegacySuspensionInterruption()
            AudioSystemStub.postSuspensionReasonInterruption()
            XCTAssertEqual(audio.state, .running)
            XCTAssertTrue(AudioSystemStub.isRunning())
            XCTAssertFalse(audio.requiresExplicitLocalResume)
            XCTAssertEqual(AudioSystemStub.startCount(), 1)
        }
    }

    /// User 2026-11-12: the switch stays on through an interruption and receiving resumes by
    /// itself when it ends, with or without shouldResume.
    @MainActor func testInterruptionEndResumesWithAndWithoutShouldResume() async {
        await withAudio { audio in
            await audio.setLocalIntent(playback: true, microphone: false)
            await audio.waitForTransition()
            postInterruption(.began)
            XCTAssertTrue(audio.playbackEnabled, "the switch stays on")
            postInterruption(.ended, extra: [AVAudioSessionInterruptionOptionKey:
                NSNumber(value: AVAudioSession.InterruptionOptions.shouldResume.rawValue)])
            await audio.waitForTransition()
            XCTAssertEqual(audio.state, .running)
            postInterruption(.began)
            postInterruption(.ended)
            await audio.waitForTransition()
            XCTAssertEqual(audio.state, .running, "no shouldResume: resumes too")
            XCTAssertFalse(audio.requiresExplicitLocalResume)
            XCTAssertTrue(AudioSystemStub.isRunning())
        }
    }

    /// No end at all (Continuity Camera, iPhone log 2026-11-12): back in front, the switch runs.
    @MainActor func testBackInFrontResumesAnInterruptionThatNeverEnded() async {
        await withAudio { audio in
            await audio.setLocalIntent(playback: true, microphone: false)
            await audio.waitForTransition()
            postInterruption(.began)
            XCTAssertEqual(audio.state, .interrupted)
            NotificationCenter.default.post(name: UIApplication.didBecomeActiveNotification, object: nil)
            await audio.waitForTransition()
            XCTAssertEqual(audio.state, .running)
            XCTAssertTrue(AudioSystemStub.isRunning())
        }
    }

    /// The end arrives while another app's audio still holds the session (setActive fails with
    /// cannotInterruptOthers in the background): the switch stays on and waits, then runs once
    /// the app is in front.
    @MainActor func testOtherAudioHoldingTheSessionWaitsForTheNextTurn() async {
        await withAudio { audio in
            await audio.setLocalIntent(playback: true, microphone: false)
            await audio.waitForTransition()
            postInterruption(.began)
            AudioSystemStub.failNextActivation(withCode: AVAudioSession.ErrorCode.cannotInterruptOthers.rawValue)
            postInterruption(.ended)
            await audio.waitForTransition()
            XCTAssertEqual(audio.state, .interrupted, "still waiting, not failed")
            XCTAssertTrue(audio.playbackEnabled)
            XCTAssertFalse(AudioSystemStub.isRunning())
            NotificationCenter.default.post(name: UIApplication.didBecomeActiveNotification, object: nil)
            await audio.waitForTransition()
            XCTAssertEqual(audio.state, .running)
        }
    }

    @MainActor func testRebuildPauseAndMicrophoneChangeKeepSessionActive() async {
        await withAudio { audio in
            await audio.setLocalIntent(playback: true, microphone: false)
            await audio.waitForTransition()
            NotificationCenter.default.post(name: AVAudioSession.routeChangeNotification, object: nil,
                userInfo: [AVAudioSessionRouteChangeReasonKey:
                    NSNumber(value: AVAudioSession.RouteChangeReason.newDeviceAvailable.rawValue)])
            await audio.waitForTransition()
            audio.setSharingPaused(true)
            XCTAssertEqual(audio.state, .paused)
            XCTAssertFalse(AudioSystemStub.isRunning())
            audio.resumeForCore()
            await audio.waitForTransition()
            await audio.setLocalIntent(playback: true, microphone: true)
            await audio.waitForTransition()
            XCTAssertEqual(audio.state, .running)
            XCTAssertEqual(AudioSystemStub.startCount(), 4)
            XCTAssertEqual(AudioSystemStub.activationCount(), 1)
            XCTAssertEqual(AudioSystemStub.deactivationCount(), 0)
            audio.stopFromPhone()
            XCTAssertFalse(AudioSystemStub.isRunning())
            XCTAssertEqual(AudioSystemStub.deactivationCount(), 1)
        }
    }

    /// L16: the microphone permission is already denied while the speaker
    /// plays. Turning the microphone on reports the denial and leaves the
    /// playing speaker alone (no permission sheet, no restart).
    @MainActor func testDeniedMicrophoneReportsAndKeepsPlaybackRunning() async {
        await withAudio { audio in
            await audio.setLocalIntent(playback: true, microphone: false)
            await audio.waitForTransition()
            let starts = AudioSystemStub.startCount()
            AudioSystemStub.setPermissionDenied()
            let request = Task { await audio.setLocalIntent(playback: true, microphone: true) }
            for _ in 0..<100 where !AudioSystemStub.hasPermissionRequest() { await Task.yield() }
            XCTAssertFalse(AudioSystemStub.hasPermissionRequest(), "a denied permission has no sheet to wait for")
            if AudioSystemStub.hasPermissionRequest() { AudioSystemStub.resolvePermission(false) }
            let outcome = await request.value
            await audio.waitForTransition()
            XCTAssertEqual(outcome, .microphoneDenied)
            XCTAssertFalse(audio.microphoneEnabled)
            XCTAssertEqual(audio.state, .running)
            XCTAssertTrue(AudioSystemStub.isRunning())
            XCTAssertEqual(AudioSystemStub.startCount(), starts, "playback was not interrupted")
        }
    }

    /// Control Center toggles run in the background, where no permission sheet
    /// can show: an undecided microphone permission changes nothing (the intent
    /// continues in the app to ask), a denied one is reported, a granted one
    /// turns the microphone on.
    @MainActor func testControlIntentNeverAsksForThePermission() async {
        await withAudio { audio in
            AudioSystemStub.setPermissionGranted(false)
            let undecided = await audio.setControlIntent(playback: false, microphone: true)
            XCTAssertEqual(undecided, .microphonePermissionNeeded)
            XCTAssertFalse(AudioSystemStub.hasPermissionRequest())
            XCTAssertFalse(audio.microphoneEnabled)
            XCTAssertEqual(audio.state, .stopped)

            AudioSystemStub.setPermissionDenied()
            let denied = await audio.setControlIntent(playback: false, microphone: true)
            XCTAssertEqual(denied, .microphoneDenied)
            XCTAssertFalse(AudioSystemStub.hasPermissionRequest())
            XCTAssertFalse(audio.microphoneEnabled)

            AudioSystemStub.setPermissionGranted(true)
            let granted = await audio.setControlIntent(playback: false, microphone: true)
            XCTAssertEqual(granted, .applied)
            XCTAssertTrue(audio.microphoneEnabled)
            await audio.waitUntilSettled(limit: .seconds(5))
            XCTAssertEqual(audio.state, .running)
        }
    }

    /// A toggle holds the session until its switch runs (or definitely does
    /// not); switching everything off is settled at once.
    @MainActor func testControlIntentSettlesWhenTheSpeakerRuns() async {
        await withAudio { audio in
            let on = await audio.setControlIntent(playback: true, microphone: false)
            XCTAssertEqual(on, .applied)
            XCTAssertTrue(audio.playbackEnabled)
            await audio.waitUntilSettled(limit: .seconds(5))
            XCTAssertEqual(audio.state, .running)
            XCTAssertTrue(AudioSystemStub.isRunning())

            let off = await audio.setControlIntent(playback: false, microphone: false)
            XCTAssertEqual(off, .applied)
            await audio.waitUntilSettled(limit: .seconds(5))
            XCTAssertEqual(audio.state, .stopped)
            XCTAssertFalse(AudioSystemStub.isRunning())
        }
    }

    @MainActor private func waitForPermissionRequest() async throws {
        for _ in 0..<1000 {
            if AudioSystemStub.hasPermissionRequest() { return }
            try await Task.sleep(for: .milliseconds(1))
        }
        XCTFail("Permission request was not issued")
    }

    @MainActor func testPermissionResolutionStartsOnlyFinalIntent() async throws {
        try await withAudio { audio in
            AudioSystemStub.setPermissionGranted(false)
            var intents: [(Bool, Bool)] = []
            audio.intentChanged = { intents.append(($0, $1)) }
            let request = Task { await audio.setLocalIntent(playback: true, microphone: true) }
            try await waitForPermissionRequest()
            XCTAssertEqual(AudioSystemStub.startCount(), 0)
            XCTAssertTrue(intents.isEmpty)
            AudioSystemStub.resolvePermission(true)
            await request.value
            await audio.waitForTransition()
            XCTAssertEqual(AudioSystemStub.startCount(), 1)
            XCTAssertEqual(intents.count, 1)
            XCTAssertTrue(intents[0].0 && intents[0].1)
        }
    }

    @MainActor func testPermissionReplyCannotRestartAfterStop() async throws {
        try await withAudio { audio in
            AudioSystemStub.setPermissionGranted(false)
            let request = Task { await audio.setLocalIntent(playback: true, microphone: true) }
            try await waitForPermissionRequest()
            audio.stopFromPhone()
            AudioSystemStub.resolvePermission(true)
            await request.value
            XCTAssertEqual(audio.state, .stopped)
            XCTAssertFalse(audio.playbackEnabled)
            XCTAssertFalse(audio.microphoneEnabled)
            XCTAssertEqual(AudioSystemStub.startCount(), 0)
        }
    }

    private static let groupFixture = "owFQM2twUP3+RC70Hfl/+1ddhwKCowECA6IBAQOiAWdlZDI1NTE5AlggA6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbgEowGAAgAEoKMBGRMBA6IBAQOiAWdlZDI1NTE5AlggA6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbgEoQGBowGiAWdlZDI1NTE5AlggA6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbgCAAMAA6IBUDNrcFD9/kQu9B35f/tXXYcDgaEBqAEAAlAza3BQ/f5ELvQd+X/7V12HA6IBZ2VkMjU1MTkCWCADoQe/884Qvh1w3RjnS8CZZ+TWMJulDV8d3IZkElUxuAQABaIBZ2VkMjU1MTkCWCADoQe/884Qvh1w3RjnS8CZZ+TWMJulDV8d3IZkElUxuAduaU9TIGF1ZGlvIHRlc3QIY2lvcwkB"

    /// A data directory holding the fixture group state.
    private func ownerRoot() throws -> URL {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        // The state is a test input, rather than a product resource. Embedding
        // the small fixture keeps the host independent of Xcode's resource
        // phase and still exercises MobileSession's real group-state.v2 load.
        let fixture = try XCTUnwrap(Data(base64Encoded: Self.groupFixture))
        try fixture.write(to: DeviceOnlyStorage.prepare(in: root).appendingPathComponent("group-state.v2"))
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        return root
    }

    private func startCore(_ root: URL) async throws -> MobileSession {
        let config = MobileConfig(dataDir: root.path, cacheDir: root.appendingPathComponent("Cache").path,
            logDir: root.appendingPathComponent("Logs").path, deviceName: "Audio lifecycle test",
            platform: .ios, profile: .mobile)
        let core = try await MobileSession.start(config: config, discovery: MobileDiscovery(),
            identitySeed: Data((0..<32).map { UInt8($0) }))
        // The core grants the speaker only while this phone holds the
        // group's audio-exit claim, which needs the speaker_receive fact
        // that CoreSession advertises from the native route.
        core.updatePlatformFacts(facts: MobilePlatformFacts(microphoneSend: false,
            speakerReceive: true, speakerSend: false, privilegedInputReady: false))
        return core
    }

    @MainActor private func withOwner(_ body: (MobileAudioOwner, MobileSession) async throws -> Void) async throws {
        AudioSystemStub.install()
        defer { AudioSystemStub.uninstall() }
        let core = try await startCore(ownerRoot())
        let owner = MobileAudioOwner(media: core.media())
        await owner.subscribe()
        do {
            try await body(owner, core)
        } catch {
            await owner.shutdown()
            try? await core.shutdown()
            throw error
        }
        await owner.shutdown()
        try await core.shutdown()
    }

    /// Native audio starts only for directions the Rust snapshot grants, and
    /// that grant reaches the owner through the media subscription on a later
    /// main-queue turn, so a settled transition can still be the pre-grant stop.
    @MainActor private func waitForRunning(_ audio: MobileAudioSession) async throws {
        for _ in 0..<1000 {
            await audio.waitForTransition()
            if audio.state == .running { return }
            try await Task.sleep(for: .milliseconds(1))
        }
        XCTFail("Audio did not reach running; last state \(audio.state)")
    }

    @MainActor func testOwnerPhoneResumeClearsInterruptedFailure() async throws {
        try await withOwner { owner, core in
            await owner.setLocalIntent(playback: true, microphone: false)
            try await waitForRunning(owner.audio)
            postInterruption(.began)
            XCTAssertEqual(owner.audio.state, .interrupted)
            XCTAssertEqual(core.media().snapshot().failures[0], .interrupted)
            await owner.resumeFromPhone()
            XCTAssertEqual(core.media().snapshot().failures[0], .none)
            try await waitForRunning(owner.audio)
            XCTAssertFalse(owner.audio.requiresExplicitLocalResume)
        }
    }

    @MainActor func testPermissionSnapshotTracksAuthorizationAndSurvivesStop() async throws {
        try await withOwner { owner, core in
            XCTAssertEqual(core.media().snapshot().microphonePermission, .granted)
            AudioSystemStub.setPermissionGranted(false)
            NotificationCenter.default.post(name: UIApplication.didBecomeActiveNotification, object: nil)
            await Task.yield()
            XCTAssertEqual(core.media().snapshot().microphonePermission, .notDetermined)
            let request = Task { await owner.setLocalIntent(playback: false, microphone: true) }
            try await waitForPermissionRequest()
            AudioSystemStub.resolvePermission(false)
            await request.value
            XCTAssertEqual(core.media().snapshot().microphonePermission, .denied)
            owner.stopFromPhone()
            XCTAssertEqual(core.media().snapshot().microphonePermission, .denied)
            AudioSystemStub.setPermissionGranted(true)
            NotificationCenter.default.post(name: UIApplication.didBecomeActiveNotification, object: nil)
            await Task.yield()
            XCTAssertEqual(core.media().snapshot().microphonePermission, .granted)
        }
    }

    /// iOS (user 2026-11-12): the microphone captures as soon as it is on,
    /// without any computer taking it; the core sends nothing until one does.
    @MainActor func testIOSMicrophoneCapturesWithoutDemand() async throws {
        try await withOwner { owner, core in
            await owner.setLocalIntent(playback: false, microphone: true)
            try await waitForRunning(owner.audio)
            let snapshot = core.media().snapshot()
            XCTAssertEqual(snapshot.localEnabledMask & 2, 2)
            XCTAssertEqual(snapshot.runningMask & 2, 2, "captures with no peer in the group")
            XCTAssertTrue(owner.audio.keepsBackgroundSession)
            await owner.setLocalIntent(playback: false, microphone: false)
            await owner.audio.waitForTransition()
            XCTAssertFalse(AudioSystemStub.isRunning())
        }
    }

    /// H12: an OS interruption stops audio, so nothing renders and iOS
    /// suspends the process. The network owner (and its App Group file lock)
    /// must not be retained for it.
    @MainActor func testInterruptedAudioDoesNotKeepTheBackgroundSession() async {
        await withAudio { audio in
            await audio.setLocalIntent(playback: true, microphone: false)
            await audio.waitForTransition()
            XCTAssertTrue(audio.keepsBackgroundSession)
            postInterruption(.began)
            XCTAssertEqual(audio.state, .interrupted)
            XCTAssertFalse(audio.keepsBackgroundSession)
        }
    }

    /// M26: releasing the network owner (going to background) keeps the
    /// phone's accepted speaker/microphone switches.
    @MainActor func testOwnerShutdownKeepsTheAcceptedIntent() async throws {
        try await withOwner { owner, core in
            await owner.setLocalIntent(playback: true, microphone: false)
            try await waitForRunning(owner.audio)
            await owner.shutdown()
            XCTAssertTrue(owner.audio.playbackEnabled)
            XCTAssertFalse(AudioSystemStub.isRunning())
            XCTAssertEqual(core.media().snapshot().localEnabledMask & 1, 1,
                           "the switch is not turned off on the way out either")
        }
    }

    /// H12 + M26: the speaker plays in background, an interruption begins
    /// and the lifecycle releases the session (the owner's file lock). When
    /// the interruption ends (with or without shouldResume) the owner asks for
    /// a network owner; on the new session the accepted switch runs again and
    /// the core has the speaker failure cleared, so computers forward to it.
    @MainActor func testInterruptionEndAfterReleaseRestoresOnANewSession() async throws {
        AudioSystemStub.install()
        defer { AudioSystemStub.uninstall() }
        let root = try ownerRoot()
        let first = try await startCore(root)
        let owner = MobileAudioOwner(media: first.media())
        await owner.subscribe()
        await owner.setLocalIntent(playback: true, microphone: false)
        try await waitForRunning(owner.audio)

        postInterruption(.began)
        XCTAssertFalse(owner.audio.keepsBackgroundSession)
        await owner.shutdown()
        try await first.shutdown()
        XCTAssertFalse(owner.needsNetworkOwner)

        var requested = 0
        owner.eligibilityChanged = { requested += 1 }
        // Without shouldResume too (user 2026-11-12).
        postInterruption(.ended)
        await owner.audio.waitForTransition()
        XCTAssertTrue(owner.needsNetworkOwner)
        XCTAssertGreaterThan(requested, 0)
        XCTAssertTrue(owner.audio.playbackEnabled)

        let second = try await startCore(root)
        owner.bind(media: second.media())
        await owner.subscribe()
        owner.restoreAcceptedIntent()
        XCTAssertFalse(owner.needsNetworkOwner)
        try await waitForRunning(owner.audio)
        XCTAssertEqual(second.media().snapshot().localEnabledMask & 1, 1)
        XCTAssertEqual(second.media().snapshot().failures[0], .none)
        await owner.shutdown()
        try await second.shutdown()
    }
}
