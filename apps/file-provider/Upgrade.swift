import Foundation
import FileProvider
import Darwin

// The marker lives beside FileProvider, outside the tree it protects. Both the
// extension and manager use the short local lock; only managers hold the reset
// lock while awaiting fileproviderd (which must be able to enumerate emptiness).
struct ProviderUpgrade {
    let group: URL
    var root: URL { group.appendingPathComponent("FileProvider", isDirectory: true) }
    private let marker = ".received-links-reuse-v1"
    private func failure() -> Error { NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }

    private func directory(_ url: URL) throws -> Int32 {
        let fd = open(url.path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        guard fd >= 0 else { throw failure() }
        return fd
    }
    private func lock(_ name: String) throws -> Int32 {
        let parent = try directory(group); defer { close(parent) }
        let fd = openat(parent, name, O_CREAT | O_RDWR | O_NOFOLLOW | O_CLOEXEC, 0o600)
        guard fd >= 0 else { throw failure() }
        var info = stat()
        guard fstat(fd, &info) == 0, info.st_mode & S_IFMT == S_IFREG,
              info.st_nlink == 1, flock(fd, LOCK_EX) == 0 else {
            close(fd); throw CocoaError(.fileWriteNoPermission)
        }
        return fd
    }
    func withLocalLock<T>(_ operation: () throws -> T) throws -> T {
        let fd = try lock(marker + ".lock"); defer { close(fd) }
        return try operation()
    }
    private func readMarker(_ name: String) throws -> String? {
        let parent = try directory(group); defer { close(parent) }
        let fd = openat(parent, name, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
        if fd < 0 && errno == ENOENT { return nil }
        guard fd >= 0 else { throw failure() }
        defer { close(fd) }
        var info = stat()
        guard fstat(fd, &info) == 0, info.st_mode & S_IFMT == S_IFREG,
              info.st_nlink == 1, info.st_size <= 32 else { throw CocoaError(.fileReadCorruptFile) }
        var bytes = [UInt8](repeating: 0, count: 32)
        let count = read(fd, &bytes, bytes.count)
        guard count >= 0, let value = String(bytes: bytes.prefix(count), encoding: .utf8) else { throw CocoaError(.fileReadCorruptFile) }
        return value
    }
    private func phase() throws -> String? {
        let value = try readMarker(marker)
        guard value == nil || ["wiping", "reset", "complete"].contains(value!) else { throw CocoaError(.fileReadCorruptFile) }
        return value
    }
    private func persist(_ value: String, name: String? = nil) throws {
        let name = name ?? marker
        let parent = try directory(group); defer { close(parent) }
        let temporary = name + ".tmp-" + UUID().uuidString
        let fd = openat(parent, temporary, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
        guard fd >= 0 else { throw failure() }
        defer { close(fd); unlinkat(parent, temporary, 0) }
        let bytes = Array(value.utf8)
        guard write(fd, bytes, bytes.count) == bytes.count, fsync(fd) == 0,
              renameat(parent, temporary, parent, name) == 0, fsync(parent) == 0 else { throw failure() }
    }
    // Walk pinned directories, unlink links themselves, and reject mount points.
    // No chmod/chflags: a hard link may refer to a source file outside our store.
    private func clear(_ fd: Int32, device: dev_t) throws {
        guard let stream = fdopendir(dup(fd)) else { throw failure() }
        defer { closedir(stream) }
        while true {
            errno = 0
            guard let entry = readdir(stream) else {
                guard errno == 0 else { throw failure() }
                break
            }
            let name = withUnsafePointer(to: &entry.pointee.d_name) {
                $0.withMemoryRebound(to: CChar.self, capacity: Int(MAXNAMLEN) + 1) { String(cString: $0) }
            }
            if name == "." || name == ".." { continue }
            var info = stat()
            guard fstatat(fd, name, &info, AT_SYMLINK_NOFOLLOW) == 0 else { throw failure() }
            if info.st_mode & S_IFMT == S_IFDIR {
                let child = openat(fd, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
                guard child >= 0 else { throw failure() }
                defer { close(child) }
                var pinned = stat()
                guard fstat(child, &pinned) == 0, pinned.st_dev == device,
                      pinned.st_ino == info.st_ino else { throw CocoaError(.fileWriteNoPermission) }
                try clear(child, device: device)
                guard unlinkat(fd, name, AT_REMOVEDIR) == 0 else { throw failure() }
            } else {
                guard unlinkat(fd, name, 0) == 0 else { throw failure() }
            }
        }
        guard fsync(fd) == 0 else { throw failure() }
    }
    // Injection points model process loss; no phase is acknowledged in advance.
    func prepareLocal(resetToken: String? = nil, checkpoint: (String) throws -> Void = { _ in }) throws {
        try withLocalLock {
            let current = try phase()
            var force = false
            if let resetToken {
                guard resetToken.count == 32, resetToken.allSatisfy({ $0.isHexDigit && $0.isASCII }) else { throw CocoaError(.fileReadCorruptFile) }
                force = try readMarker(marker + ".reset-token") != resetToken
            }
            if !force && (current == "complete" || current == "reset") { return }
            // Persist the closed gate before the new reset identity. Retrying
            // the same identity after completion must preserve newly added files.
            try persist("wiping")
            if force, let resetToken { try persist(resetToken, name: marker + ".reset-token") }
            try checkpoint("wiping")
            let parent = try directory(group); defer { close(parent) }
            if mkdirat(parent, "FileProvider", 0o700) != 0 && errno != EEXIST { throw failure() }
            let fd = openat(parent, "FileProvider", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
            guard fd >= 0 else { throw failure() }
            defer { close(fd) }
            var info = stat(), parentInfo = stat()
            guard fstat(fd, &info) == 0, fstat(parent, &parentInfo) == 0,
                  info.st_dev == parentInfo.st_dev else { throw CocoaError(.fileWriteNoPermission) }
            try clear(fd, device: info.st_dev)
            try checkpoint("cleared")
            try persist("reset")
            try checkpoint("reset")
        }
    }
    func pending() throws -> Bool { try phase().map { $0 != "complete" } ?? false }
    func requireReady() throws {
        guard try phase() == "complete" else { throw NSFileProviderError(.serverUnreachable) }
    }
    @discardableResult
    func finish(registered: Bool, enabled: Bool, connected: Bool,
                reset: () async throws -> Void,
                checkpoint: (String) throws -> Void = { _ in }) async throws -> Bool {
        let fd = try lock(marker + ".reset.lock"); defer { close(fd) }
        try prepareLocal()
        if try !pending() { return false }
        if registered {
            guard enabled, connected else { throw NSFileProviderError(.serverUnreachable) }
            try await reset()
        }
        try checkpoint("system-reset")
        try withLocalLock { try persist("complete") }
        return true
    }
}

// Fixtures with arbitrary names do not implicitly become upgrade targets.
func providerUpgrade(in root: URL) -> ProviderUpgrade? {
    root.lastPathComponent == "FileProvider" ? ProviderUpgrade(group: root.deletingLastPathComponent()) : nil
}
@discardableResult
func finishProviderUpgrade(_ upgrade: ProviderUpgrade, domain: NSFileProviderDomain?,
                           manager: NSFileProviderManager? = nil) async throws -> Bool {
    return try await upgrade.finish(registered: domain != nil, enabled: domain?.userEnabled ?? false,
                             connected: domain?.isDisconnected == false || manager != nil) {
        guard let manager = manager ?? domain.flatMap({ NSFileProviderManager(for: $0) }) else {
            throw CocoaError(.fileReadUnknown)
        }
        // Replicated providers only honor working-set signals. Expired old
        // anchors and the now-empty catalog force a full reconciliation.
        try await manager.signalEnumerator(for: .workingSet)
        // Reuse the existing launch recovery barrier, once per cut-over. Apple
        // recommends this expensive API only for testing/debugging; signed
        // upgrade acceptance must measure this bounded startup operation.
        try await manager.waitForStabilization()
        let observer = MaterializedObserver(manager.enumeratorForMaterializedItems())
        defer { observer.enumerator.invalidate() }
        let items = try await observer.read()
        guard items.allSatisfy({ $0.itemIdentifier == .rootContainer || $0.itemIdentifier == .trashContainer }) else {
            throw NSFileProviderError(.cannotSynchronize)
        }
    }
}
