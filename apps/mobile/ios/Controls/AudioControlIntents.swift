import AppIntents
import Foundation

// Control Center toggles 扬声器 / 麦克风 and the Live Activities' stop buttons.
//
// These types are compiled into the app and into the OinkControls widget
// extension (Controls/), because `ControlWidgetToggle(action:)` and
// `Button(intent:)` in the extension must name them. They are performed in
// the app's process only: the system runs an intent that conforms to
// AudioPlaybackIntent or LiveActivityIntent in the app, launching it in the
// background when it is not running ("Adding interactivity to widgets and
// Live Activities"). The extension's copy never runs; its perform throws.
//
// Never in Shared/: that would compile them into the share extension, and
// with the app not running linkd picked the share extension to perform an
// action it also declared (see ReceiveClipboardIntent.swift). They are not
// discoverable either, so Shortcuts and Siri never offer them and only the
// controls and the Live Activities run them.

/// 扬声器: this phone is the group's audio exit (it plays what the other
/// devices send) or not. Works without a group connection: the native
/// speaker follows this phone's own choice and the sound arrives once a
/// device sends it.
struct SetSpeakerIntent: SetValueIntent, AudioPlaybackIntent, LiveActivityIntent {
    static let title: LocalizedStringResource = "扬声器"
    static let description = IntentDescription("把本机作为音频出口。")
    static let isDiscoverable = false

    @available(iOS 26.0, *)
    static var supportedModes: IntentModes { .background }

    @Parameter(title: "扬声器")
    var value: Bool

    init() {}
    init(value: Bool) { self.value = value }

    @MainActor
    func perform() async throws -> some IntentResult {
        #if OINK_MAIN_APP
        try await AudioControlActions.setSpeaker(value)
        #else
        throw ControlIntentError.notInApp
        #endif
        return .result()
    }
}

/// 麦克风: this phone shares its microphone or not. On iOS the microphone
/// captures as soon as it is on and sends only while a computer takes it
/// (user 2026-11-12); recording from the background needs the Live Activity
/// (AudioRecordingIntent), started from LiveActivityIntent's perform. The
/// permission sheet cannot show in the background: an undecided permission
/// continues in the app (`AudioControlActions.setMicrophone`).
struct SetMicrophoneIntent: SetValueIntent, AudioRecordingIntent, LiveActivityIntent {
    static let title: LocalizedStringResource = "麦克风"
    static let description = IntentDescription("共享本机麦克风。")
    static let isDiscoverable = false

    @available(iOS 26.0, *)
    static var supportedModes: IntentModes { [.background, .foreground(.dynamic)] }

    @Parameter(title: "麦克风")
    var value: Bool

    init() {}
    init(value: Bool) { self.value = value }

    @MainActor
    func perform() async throws -> some IntentResult {
        #if OINK_MAIN_APP
        try await AudioControlActions.setMicrophone(value, intent: self)
        #else
        throw ControlIntentError.notInApp
        #endif
        return .result()
    }
}

#if OINK_MAIN_APP
// iOS 18–25 continue an intent in the app through ForegroundContinuableIntent
// (iOS 26 uses `.foreground(.dynamic)` above and `continueInForeground`).
// The protocol is unavailable in app extensions, so only the app's copy
// conforms; the extension's copy never performs.
extension SetMicrophoneIntent: ForegroundContinuableIntent {}
#endif

/// Why a control could not apply; Control Center shows the message.
struct ControlIntentError: Error, LocalizedError, CustomLocalizedStringResourceConvertible {
    let message: String
    var errorDescription: String? { message }
    var localizedStringResource: LocalizedStringResource { LocalizedStringResource(stringLiteral: message) }

    /// The extension's copy of an intent ran (the system routes them to the app).
    static let notInApp = ControlIntentError(message: "请打开 Oink 后再试。")
}

/// ControlWidgetTemplateBuilder accepts one fixed button type. This intent
/// continues in Oink only for start; its stop parameter stays in the background.
struct ForwardButtonIntent: AudioPlaybackIntent, LiveActivityIntent {
    static let title: LocalizedStringResource = "音频转发"
    static let isDiscoverable = false
    @available(iOS 26.0, *)
    static var supportedModes: IntentModes { [.background, .foreground(.dynamic)] }

    @Parameter(title: "停止转发")
    var stopping: Bool
    init() { stopping = false }
    init(stopping: Bool) { self.stopping = stopping }

    @MainActor
    func perform() async throws -> some IntentResult {
        #if OINK_MAIN_APP
        if stopping {
            ForwardControlStore.stop()
            await AudioControlActions.endForwardActivity()
            return .result()
        }
        if #available(iOS 26.0, *) {
            try await continueInForeground(alwaysConfirm: false)
        } else {
            try await requestToContinueInForeground()
        }
        ForwardLaunchRequest.shared.pending = true
        ForwardLaunchRequest.shared.presentIfReady()
        #else
        throw ControlIntentError.notInApp
        #endif
        return .result()
    }
}

struct StopForwardIntent: AudioPlaybackIntent, LiveActivityIntent {
    static let title: LocalizedStringResource = "停止转发"
    static let isDiscoverable = false
    @available(iOS 26.0, *)
    static var supportedModes: IntentModes { .background }

    @MainActor
    func perform() async throws -> some IntentResult {
        ForwardControlStore.stop()
        #if OINK_MAIN_APP
        await AudioControlActions.endForwardActivity()
        #endif
        return .result()
    }
}

#if OINK_MAIN_APP
extension ForwardButtonIntent: ForegroundContinuableIntent {}
#endif
