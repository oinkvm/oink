import AppIntents
import SwiftUI
import WidgetKit

/// OinkControls: Control Center toggles 扬声器 / 麦克风 and the Live
/// Activities of a running speaker exit or shared microphone (user
/// 2026-11-12, mockup B). No Rust core here: the toggles read the App Group
/// state the app writes (`ControlsStore`), and their intents run in the app.
@main
struct OinkControlsBundle: WidgetBundle {
    var body: some Widget {
        SpeakerControl()
        MicrophoneControl()
        ForwardControl()
        AudioActivityWidget()
    }
}

/// The toggle's value: on, and the theme accent it is tinted with.
struct AudioControlValue {
    var isOn: Bool
    var accent: UInt32
    var forwarding = false
}

struct AudioControlValueProvider: ControlValueProvider {
    let role: AudioActivityText.Role

    var previewValue: AudioControlValue { AudioControlValue(isOn: false, accent: ControlsState.defaultAccent) }

    func currentValue() async throws -> AudioControlValue {
        let state = ControlsStore.directory().map(ControlsStore.current(in:)) ?? ControlsState()
        return AudioControlValue(isOn: role == .speaker ? state.speaker : state.microphone, accent: state.accent, forwarding: state.forwarding)
    }
}

/// 扬声器: on = this phone is the group's audio exit (receives audio). The
/// glyphs are the device list's: speaker, plus the receive badge while on.
struct SpeakerControl: ControlWidget {
    var body: some ControlWidgetConfiguration {
        StaticControlConfiguration(kind: ControlKind.speaker, provider: AudioControlValueProvider(role: .speaker)) { value in
            ControlWidgetToggle("扬声器", isOn: value.isOn, action: SetSpeakerIntent()) { isOn in
                Label(value.forwarding ? "转发中" : isOn ? "接收音频" : "关", image: isOn ? "oink.speaker.receive" : "oink.speaker")
            }
            .tint(Color(rgb: value.accent))
        }
        .displayName("扬声器")
        .description("把本机作为音频出口")
    }
}

/// 麦克风: on = this phone shares its microphone (send badge while on).
struct MicrophoneControl: ControlWidget {
    var body: some ControlWidgetConfiguration {
        StaticControlConfiguration(kind: ControlKind.microphone, provider: AudioControlValueProvider(role: .microphone)) { value in
            ControlWidgetToggle("麦克风", isOn: value.isOn, action: SetMicrophoneIntent()) { isOn in
                Label(isOn ? "共享中" : "关", image: isOn ? "oink.microphone.send" : "oink.microphone")
            }
            .tint(Color(rgb: value.accent))
        }
        .displayName("麦克风")
        .description("共享本机麦克风")
    }
}

extension Color {
    /// 0xRRGGBB.
    init(rgb: UInt32) {
        self.init(red: Double((rgb >> 16) & 0xFF) / 255, green: Double((rgb >> 8) & 0xFF) / 255, blue: Double(rgb & 0xFF) / 255)
    }
}

/// System button styling, with no toggle highlight. Its action is selected
/// from the actual broadcast state every time Control Center asks for a value.
struct ForwardControl: ControlWidget {
    var body: some ControlWidgetConfiguration {
        StaticControlConfiguration(kind: ControlKind.forwarding, provider: AudioControlValueProvider(role: .speaker)) { value in
            ControlWidgetButton(action: ForwardButtonIntent(stopping: value.forwarding)) {
                Label(value.forwarding ? "转发中" : "音频转发", image: "oink.speaker.send")
            }
        }
        .displayName("音频转发")
        .description("把本机声音转发到音频出口")
    }
}
