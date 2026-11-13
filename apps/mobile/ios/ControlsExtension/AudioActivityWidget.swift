import ActivityKit
import AppIntents
import SwiftUI
import WidgetKit

/// Live Activity of a running speaker exit (正在作为音频出口) or shared
/// microphone (麦克风共享中): lock screen card, Dynamic Island compact /
/// minimal and the long-press expanded view, each with a stop button that
/// switches the role off like the Control Center toggle (mockup B). Minimal
/// on purpose: no duration or battery.
struct AudioActivityWidget: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: OinkAudioActivityAttributes.self) { context in
            LockScreenCard(role: context.attributes.role, state: context.state, stale: context.isStale)
                .activityBackgroundTint(Color(red: 28 / 255, green: 28 / 255, blue: 30 / 255).opacity(0.62))
                .activitySystemActionForegroundColor(.white)
        } dynamicIsland: { context in
            let role = context.attributes.role
            let accent = Color(rgb: context.state.accent)
            return DynamicIsland {
                DynamicIslandExpandedRegion(.leading) {
                    RoleGlyph(role: role, forwarding: context.state.forwarding, size: 28).foregroundStyle(role == .speaker && context.state.forwarding ? Color(rgb: 0x2FBF71) : accent).padding(.leading, 4)
                }
                DynamicIslandExpandedRegion(.center) {
                    RoleText(role: role, state: context.state, stale: context.isStale)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                DynamicIslandExpandedRegion(.trailing) {
                    if !context.isStale { LevelBars(color: accent).padding(.trailing, 4) }
                }
                DynamicIslandExpandedRegion(.bottom) {
                    HStack {
                        HStack(spacing: 6) {
                            Image("OinkMark").resizable().frame(width: 15, height: 15)
                            Text("Oink").font(.system(size: 13))
                        }
                        .foregroundStyle(.white.opacity(0.6))
                        Spacer(minLength: 0)
                        StopButton(role: role, forwarding: context.state.forwarding) {
                            HStack(spacing: 6) {
                                Image(systemName: "stop.fill").font(.system(size: 12))
                                Text(AudioActivityText.stop(role, forwarding: context.state.forwarding)).font(.system(size: 15, weight: .semibold))
                            }
                            .foregroundStyle(.white)
                            .padding(.horizontal, 16)
                            .frame(height: 38)
                            .background(Capsule().fill(accent))
                        }
                    }
                    .padding(.horizontal, 4)
                }
            } compactLeading: {
                RoleGlyph(role: role, forwarding: context.state.forwarding, size: 18).foregroundStyle(role == .speaker && context.state.forwarding ? Color(rgb: 0x2FBF71) : accent)
            } compactTrailing: {
                if !context.isStale { LevelBars(color: accent) }
            } minimal: {
                RoleGlyph(role: role, forwarding: context.state.forwarding, size: 16).foregroundStyle(role == .speaker && context.state.forwarding ? Color(rgb: 0x2FBF71) : accent)
            }
            .keylineTint(accent)
        }
    }
}

/// Lock screen: app icon, title and peers, round stop button.
private struct LockScreenCard: View {
    let role: AudioActivityText.Role
    let state: OinkAudioActivityAttributes.ContentState
    var stale = false

    var body: some View {
        let accent = Color(rgb: state.accent)
        HStack(spacing: 12) {
            Image("OinkMark").resizable().frame(width: 25, height: 25)
                .foregroundStyle(.white)
                .frame(width: 40, height: 40)
                .background(accent, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
                .accessibilityHidden(true)
            RoleText(role: role, state: state, stale: stale)
            Spacer(minLength: 0)
            StopButton(role: role, forwarding: state.forwarding) {
                Image(systemName: "stop.fill")
                    .font(.system(size: 15))
                    .foregroundStyle(accent)
                    .frame(width: 44, height: 44)
                    .background(Circle().fill(.white.opacity(0.18)))
            }
        }
        .padding(16)
    }
}

private struct RoleText: View {
    let role: AudioActivityText.Role
    let state: OinkAudioActivityAttributes.ContentState
    var stale = false

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(stale ? "请打开 Oink 更新状态" : AudioActivityText.title(role, forwarding: state.forwarding, paused: state.paused)).font(.system(size: 16, weight: .semibold)).foregroundStyle(.white)
            Text(AudioActivityText.subtitle(role, peers: state.peers, forwarding: state.forwarding)).font(.system(size: 13.5)).foregroundStyle(.white.opacity(0.65))
        }
        .lineLimit(1)
    }
}

/// The device list's icon of the running role: speaker + receive badge,
/// microphone + send badge (custom symbols, badge knocked out of the glyph).
private struct RoleGlyph: View {
    let role: AudioActivityText.Role
    var forwarding = false
    let size: CGFloat

    var body: some View {
        Image(role == .speaker ? (forwarding ? "oink.speaker.send" : "oink.speaker.receive") : "oink.microphone.send")
            .font(.system(size: size))
            .accessibilityHidden(true)
    }
}

/// The mockup's level bars (static: a Live Activity does not animate audio).
private struct LevelBars: View {
    let color: Color

    var body: some View {
        HStack(spacing: 2.5) {
            ForEach(Array([6.0, 12, 9, 14, 7].enumerated()), id: \.offset) { _, height in
                Capsule().fill(color).frame(width: 3, height: height)
            }
        }
        .frame(height: 16)
        .accessibilityHidden(true)
    }
}

/// Switches the role off (the same intent as the Control Center toggle).
private struct StopButton<Content: View>: View {
    let role: AudioActivityText.Role
    var forwarding = false
    @ViewBuilder let content: () -> Content

    var body: some View {
        Group {
            switch role {
            case .speaker:
                if forwarding { Button(intent: StopForwardIntent(), label: content) }
                else { Button(intent: SetSpeakerIntent(value: false), label: content) }
            case .microphone: Button(intent: SetMicrophoneIntent(value: false), label: content)
            }
        }
        .buttonStyle(.plain)
        .accessibilityLabel(AudioActivityText.stop(role, forwarding: forwarding))
    }
}
