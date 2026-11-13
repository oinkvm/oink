import Foundation

// Controls/ is compiled into the app and the OinkControls widget extension
// (Control Center toggles and Live Activities), never into the share
// extension: see AudioControlIntents.swift. The extension has no Rust core;
// State comes from the app-owned controls file and the broadcast-owned pipe.

/// What the Control Center toggles 扬声器 / 麦克风 show (iOS 18+). On means
/// the app's session lives and the phone's switch is on. There is no
/// "not connected" state: the switches work without a group (user
/// 2026-11-12), so the group connection is not part of it.
struct ControlsState: Codable, Equatable {
    var speaker = false
    var microphone = false
    var forwarding = false
    var forwardingPaused = false
    var forwardExit: String?
    /// Theme accent (0xRRGGBB, `OinkPalette.Accent.base`) the toggles are tinted with.
    var accent: UInt32 = ControlsState.defaultAccent

    /// The pink accent, the app's default.
    static let defaultAccent: UInt32 = 0xFF6B8E
}

/// Control kinds (ControlWidget `kind`, `ControlCenter.reloadControls`).
enum ControlKind {
    static let speaker = "com.hachiwii.oink.control.speaker"
    static let microphone = "com.hachiwii.oink.control.microphone"
    static let forwarding = "com.hachiwii.oink.control.forwarding"
    static let all = [speaker, microphone, forwarding]
}

/// The App Group files behind the controls: the state and the app's
/// liveness lock (`ControlsLiveness`).
enum ControlsStore {
    static let stateName = "controls.json"
    static let lockName = "controls-live.lock"

    /// The App Group data directory (the same one `SharedStore.root()` uses);
    /// nil when the container is unavailable (signing without the group).
    static func directory() -> URL? {
        guard let group = Bundle.main.object(forInfoDictionaryKey: "OinkAppGroup") as? String,
              let container = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: group) else { return nil }
        let url = container.appendingPathComponent("Library/Application Support/Oink", isDirectory: true)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true,
            attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication])
        return url
    }

    /// The written state; a missing or unreadable file is all off.
    static func read(in directory: URL) -> ControlsState {
        guard let data = try? Data(contentsOf: directory.appendingPathComponent(stateName)),
              let state = try? JSONDecoder().decode(ControlsState.self, from: data) else { return ControlsState() }
        return state
    }

    static func write(_ state: ControlsState, in directory: URL) throws {
        try JSONEncoder().encode(state).write(to: directory.appendingPathComponent(stateName),
            options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
    }

    /// App-owned roles follow its session lock. Forwarding follows the separate
    /// broadcast pipe lifetime, including when the upload extension owns networking.
    /// Kernel locks clear stale on-disk state after either process exits.
    static func current(in directory: URL) -> ControlsState {
        var state = read(in: directory)
        if !ControlsLiveness.isHeld(in: directory) {
            state.speaker = false
            state.microphone = false
        }
        state.forwarding = ForwardControlStore.active(in: directory)
        if state.forwarding {
            state.speaker = false
            if let broadcast = ForwardControlStore.read(in: directory) {
                state.forwardingPaused = broadcast.paused
                state.forwardExit = broadcast.exit
            }
        } else {
            state.forwardingPaused = false
            state.forwardExit = nil
        }
        return state
    }
}

/// The app holds an exclusive `flock` on `controls-live.lock` exactly while
/// its network session lives (the Rust owner lock's lifetime). The extension
/// tests it with a shared non-blocking lock it drops at once.
///
/// A separate file, not the Rust owner lock `mobile-session.lock`: the probe
/// takes the lock for an instant when it is free, and a session starting at
/// that instant would fail with OwnerBusy on the owner lock. Here the app's
/// exclusive lock simply waits out the probe. Like the owner lock it is
/// released before the app can be suspended (session release), because iOS
/// kills a suspended app that holds a file lock in a shared container
/// (0xdead10cc).
final class ControlsLiveness {
    private var descriptor: Int32 = -1

    var isHolding: Bool { descriptor >= 0 }

    /// The app's session started. Waits at most for a probe's instant.
    func hold(in directory: URL) {
        guard descriptor < 0 else { return }
        let fd = open(directory.appendingPathComponent(ControlsStore.lockName).path, O_RDWR | O_CREAT | O_CLOEXEC, 0o600)
        guard fd >= 0 else { return }
        guard flock(fd, LOCK_EX) == 0 else { close(fd); return }
        descriptor = fd
    }

    /// The app's session ended.
    func release() {
        guard descriptor >= 0 else { return }
        flock(descriptor, LOCK_UN)
        close(descriptor)
        descriptor = -1
    }

    /// Another open file description holds the lock (the app's session lives).
    static func isHeld(in directory: URL) -> Bool {
        let fd = open(directory.appendingPathComponent(ControlsStore.lockName).path, O_RDONLY | O_CLOEXEC)
        guard fd >= 0 else { return false }
        defer { close(fd) }
        if flock(fd, LOCK_SH | LOCK_NB) == 0 {
            flock(fd, LOCK_UN)
            return false
        }
        return errno == EWOULDBLOCK
    }

    deinit { release() }
}

/// Live Activity texts (mockup B, user 2026-11-12). `peers`: the devices
/// whose sound this phone plays (speaker) or that take its microphone.
/// When a role's Live Activity shows (user 2026-11-12): its switch is on and the app's session
/// lives, or an OS interruption released the session. Through an interruption the switch stays on
/// and the audio resumes by itself, so the card stays, also while the released app is suspended;
/// it ends when the switch goes off (also when another device takes the role), when the session
/// ends for any other reason, and when the app terminates. The Control Center toggles still read
/// off without a session: they follow the liveness lock, which a suspended app cannot hold.
enum AudioActivityRule {
    static func shows(switchOn: Bool, sessionLive: Bool, interruptionRelease: Bool) -> Bool {
        switchOn && (sessionLive || interruptionRelease)
    }
}

enum AudioActivityText {
    enum Role: String, Codable, Hashable { case speaker, microphone }

    static func title(_ role: Role, forwarding: Bool = false, paused: Bool = false) -> String {
        if role == .speaker && forwarding { return paused ? "音频转发已暂停" : "正在转发音频" }
        return switch role {
        case .speaker: "正在作为音频出口"
        case .microphone: "麦克风共享中"
        }
    }

    /// Without peers the role still runs (it does not need a connection):
    /// the speaker waits for sound, the microphone captures without sending.
    static func subtitle(_ role: Role, peers: [String], forwarding: Bool = false) -> String {
        if role == .speaker && forwarding { return peers.isEmpty ? "等待音频出口" : "出口 \(peers.joined(separator: "、"))" }
        let names = peers.joined(separator: "、")
        switch role {
        case .speaker: return peers.isEmpty ? "等待其它设备的声音" : "播放 \(names) 的声音"
        case .microphone: return peers.isEmpty ? "等待电脑使用" : "发送到 \(names)"
        }
    }

    static func stop(_ role: Role, forwarding: Bool = false) -> String {
        if role == .speaker && forwarding { return "停止转发" }
        return switch role {
        case .speaker: "停止接收"
        case .microphone: "停止共享"
        }
    }
}
