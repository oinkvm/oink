import Foundation
import CoreFoundation

struct HandoffOwner: Codable {
    let incarnation: UUID
}
struct HandoffRequest: Codable {
    enum Command: Codable {
        case receive
        case latestClip
        case devices
        case validateReceive(UInt64)
        case publishReceive(UInt64, success: Bool)
        case share(SharePayload, target: String?, waitForTarget: Bool)
        /// The transfer targets (share picker while the app owns the session).
        case targets
        /// Whether a device can be reached now (share picker, a cached device tapped).
        case confirmOnline(String)
    }
    let id: UUID
    let owner: UUID
    let command: Command
    let admitBefore: TimeInterval
    var cancelled = false
}
enum HandoffResult: Codable {
    case clipboard(ReceivedClipboard)
    case latestClip(LatestClip)
    case deliveries([DeliveryResult])
    case devices([NativeDevice])
    case targets(SendTargets)
    case valid(Bool)
    case acknowledged
}
private struct HandoffReply: Codable {
    enum State: Codable {
        case accepted
        case completed(HandoffResult)
        case failed(String)
    }
    let id: UUID
    let owner: UUID
    let state: State
}

/// A live-owner optimization. Darwin notifications are hints, not a wakeup mechanism.
/// No request is replayed on startup, and an incarnation never authorizes stealing Rust's lock.
@MainActor
final class LocalHandoff {
    static let shared = LocalHandoff()
    typealias Handler = (UUID, HandoffRequest.Command) async throws -> HandoffResult
    private struct Waiting {
        let request: HandoffRequest
        let continuation: CheckedContinuation<HandoffResult, Error>
        var accepted = false
        var admission: Task<Void, Never>?
    }
    private var owner: HandoffOwner?
    private var handler: Handler?
    private var executing: [UUID: Task<Void, Never>] = [:]
    private var waiting: [UUID: Waiting] = [:]
    private var observed = false
    private var notificationName: String { SharedStore.group + ".handoff" }

    private func observe() {
        guard !observed else { return }
        observed = true
        CFNotificationCenterAddObserver(CFNotificationCenterGetDarwinNotifyCenter(),
            Unmanaged.passUnretained(self).toOpaque(), { _, pointer, _, _, _ in
                guard let pointer else { return }
                let instance = Unmanaged<LocalHandoff>.fromOpaque(pointer).takeUnretainedValue()
                Task { @MainActor in instance.changed() }
            }, notificationName as CFString, nil, .deliverImmediately)
    }
    private func notify() {
        CFNotificationCenterPostNotification(CFNotificationCenterGetDarwinNotifyCenter(),
            CFNotificationName(notificationName as CFString), nil, nil, true)
    }
    private func directory() throws -> URL {
        let url = try SharedStore.root().appendingPathComponent("Handoff", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }
    private func file(_ id: UUID, suffix: String) throws -> URL {
        try directory().appendingPathComponent(id.uuidString + suffix)
    }
    private func write<T: Encodable>(_ value: T, to file: URL) throws {
        try JSONEncoder().encode(value).write(to: file,
            options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
    }
    private func read<T: Decodable>(_ type: T.Type, from file: URL) throws -> T {
        try JSONDecoder().decode(type, from: Data(contentsOf: file))
    }

    /// Call only after obtaining the Rust network owner, and stop before Rust shutdown.
    func serve(_ handler: @escaping Handler) throws {
        observe()
        let owner = HandoffOwner(incarnation: UUID())
        self.owner = owner
        self.handler = handler
        // Requests and replies addressed to earlier owners can never be
        // served (no durable outbox, no replay); a requester killed while
        // waiting left them behind. They are removed, never executed (L17).
        if let urls = try? FileManager.default.contentsOfDirectory(at: directory(), includingPropertiesForKeys: nil) {
            for url in urls where url.pathExtension == "request" || url.pathExtension == "reply" {
                try? FileManager.default.removeItem(at: url)
            }
        }
        try SharedStore.save(owner, name: "active-owner.json")
        notify()
    }
    func stopServing() async {
        handler = nil
        if let owner,
           (try? SharedStore.load(HandoffOwner.self, name: "active-owner.json"))?.incarnation == owner.incarnation {
            try? FileManager.default.removeItem(at: SharedStore.root().appendingPathComponent("active-owner.json"))
        }
        owner = nil
        let tasks = Array(executing.values)
        for task in tasks { task.cancel() }
        for task in tasks { await task.value }
        notify()
    }

    /// The one-second limit only bounds asking a possibly suspended owner for admission.
    /// Once accepted, the core's operation/cancellation determines the transfer lifetime.
    func request(_ command: HandoffRequest.Command, expectedOwner: UUID? = nil) async throws -> HandoffResult {
        observe()
        guard let target = try? SharedStore.load(HandoffOwner.self, name: "active-owner.json"),
              expectedOwner == nil || expectedOwner == target.incarnation else {
            throw OinkFailure(message: "原网络会话已结束，本次操作不可继续。")
        }
        let request = HandoffRequest(id: UUID(), owner: target.incarnation, command: command,
            admitBefore: ProcessInfo.processInfo.systemUptime + 1)
        defer {
            if let url = try? file(request.id, suffix: ".request") { try? FileManager.default.removeItem(at: url) }
            if let url = try? file(request.id, suffix: ".reply") { try? FileManager.default.removeItem(at: url) }
        }
        return try await withTaskCancellationHandler {
            try Task.checkCancellation()
            return try await withCheckedThrowingContinuation { continuation in
                waiting[request.id] = Waiting(request: request, continuation: continuation)
                do {
                    try write(request, to: file(request.id, suffix: ".request"))
                    waiting[request.id]?.admission = Task { @MainActor in
                        try? await Task.sleep(for: .seconds(1))
                        guard !Task.isCancelled else { return }
                        changed()
                        guard let pending = waiting[request.id], !pending.accepted else { return }
                        cancelRequest(request.id)
                        finish(request.id, result: .failure(OinkFailure(message: "当前网络 owner 未接受任务，请稍后重试。")))
                    }
                    notify()
                    changed()
                } catch { finish(request.id, result: .failure(error)) }
            }
        } onCancel: {
            Task { @MainActor in self.cancelRequest(request.id) }
        }
    }
    private func cancelRequest(_ id: UUID) {
        guard let pending = waiting[id] else { return }
        var request = pending.request
        request.cancelled = true
        try? write(request, to: file(id, suffix: ".request"))
        notify()
    }
    private func finish(_ id: UUID, result: Result<HandoffResult, Error>) {
        guard let pending = waiting.removeValue(forKey: id) else { return }
        pending.admission?.cancel()
        pending.continuation.resume(with: result)
    }
    private func changed() {
        for (id, pending) in waiting {
            guard let url = try? file(id, suffix: ".reply"),
                  let reply = try? read(HandoffReply.self, from: url), reply.owner == pending.request.owner else { continue }
            switch reply.state {
            case .accepted:
                waiting[id]?.accepted = true
                waiting[id]?.admission?.cancel()
            case .completed(let result): finish(id, result: .success(result))
            case .failed(let error): finish(id, result: .failure(OinkFailure(message: error)))
            }
        }
        guard let owner, let handler,
              let urls = try? FileManager.default.contentsOfDirectory(at: directory(), includingPropertiesForKeys: nil) else { return }
        for url in urls where url.pathExtension == "request" {
            guard let request = try? read(HandoffRequest.self, from: url), request.owner == owner.incarnation else { continue }
            let replyURL = try? file(request.id, suffix: ".reply")
            let existing = replyURL.flatMap { try? read(HandoffReply.self, from: $0) }
            if let existing {
                switch existing.state {
                case .completed, .failed: continue
                case .accepted: break
                }
            }
            if request.cancelled || (existing == nil && ProcessInfo.processInfo.systemUptime > request.admitBefore) {
                if let running = executing[request.id] { running.cancel() }
                else { reply(request, state: .failed("任务已取消。")) }
                continue
            }
            guard executing[request.id] == nil, existing == nil else { continue }
            reply(request, state: .accepted)
            executing[request.id] = Task { @MainActor in
                do {
                    try Task.checkCancellation()
                    // A finished handler's result is delivered even if the request was
                    // cancelled meanwhile: deliveries already happened, and a received
                    // clipboard's staging is released by the requester's publish report.
                    var result = try await handler(request.id, request.command)
                    if case .clipboard(var clipboard) = result {
                        clipboard.owner = request.owner
                        result = .clipboard(clipboard)
                    }
                    reply(request, state: .completed(result))
                } catch { reply(request, state: .failed(error.oinkMessage)) }
                executing.removeValue(forKey: request.id)
            }
        }
    }
    private func reply(_ request: HandoffRequest, state: HandoffReply.State) {
        try? write(HandoffReply(id: request.id, owner: request.owner, state: state), to: file(request.id, suffix: ".reply"))
        notify()
    }
}
