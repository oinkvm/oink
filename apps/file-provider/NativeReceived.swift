import Foundation
import FileProvider
import Darwin

// Content helpers have a no-progress deadline, not the short publication bound.
final class NativeActivity: @unchecked Sendable {
    private let lock = NSLock()
    private var changed = DispatchTime.now().uptimeNanoseconds
    func touch() { lock.lock(); changed = DispatchTime.now().uptimeNanoseconds; lock.unlock() }
    func expired() -> Bool {
        lock.lock(); defer { lock.unlock() }
        return DispatchTime.now().uptimeNanoseconds - changed >= 300_000_000_000
    }
}
struct NativeStoredFile: Encodable {
    let node: UInt64
    let version: UInt64
    let size: UInt64
}
struct NativeFileMetadata: Encodable {
    let size: UInt64
    let modified_ms: Int64?
    let exec_bits: UInt16?
}

// Native MOVE cleanup and symlink metadata reads must never hydrate data. Disable dataless materialization
// for this synchronous thread scope, then inspect and flush only resident data.
// Opening a file which races eviction fails instead of hydrating it.
private func withoutNativeHydration<T>(_ work: () throws -> T) throws -> T {
    let previous = getiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_THREAD)
    guard previous >= 0,
          setiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_THREAD,
                        IOPOL_MATERIALIZE_DATALESS_FILES_OFF) == 0 else { throw CocoaError(.fileReadUnknown) }
    defer { _ = setiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_THREAD, previous) }
    return try work()
}
func syncResidentNative(_ url: URL, record: Record) throws -> NativeStoredFile? {
    try withoutNativeHydration {
    if let target = record.symlink_target {
        var metadata = stat()
        guard lstat(url.path, &metadata) == 0, metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFLNK),
              metadata.st_flags & UInt32(SF_DATALESS) == 0,
              try FileManager.default.destinationOfSymbolicLink(atPath: url.path) == target,
              record.version != 0, let node = record.node else { throw CocoaError(.fileReadCorruptFile) }
        // Flush the directory entry. Opening/fsyncing the link would follow its target.
        let parent = Darwin.open(url.deletingLastPathComponent().path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        guard parent >= 0 else { throw CocoaError(.fileReadUnknown) }
        defer { close(parent) }
        guard fsync(parent) == 0 else { throw CocoaError(.fileReadUnknown) }
        return NativeStoredFile(node: node, version: record.version, size: UInt64(target.utf8.count))
    }
    var before = stat()
    guard lstat(url.path, &before) == 0,
          before.st_flags & UInt32(SF_DATALESS) == 0,
          before.st_mode & mode_t(S_IFMT) == mode_t(record.directory ? S_IFDIR : S_IFREG) else {
        throw CocoaError(.fileReadUnknown)
    }
    let fd = Darwin.open(url.path, O_RDONLY | O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { throw POSIXError(.init(rawValue: errno) ?? .EIO) }
    defer { close(fd) }
    var opened = stat()
    guard fstat(fd, &opened) == 0, before.st_dev == opened.st_dev, before.st_ino == opened.st_ino,
          opened.st_flags & UInt32(SF_DATALESS) == 0,
          opened.st_mode & mode_t(S_IFMT) == before.st_mode & mode_t(S_IFMT), record.version != 0,
          record.directory || (opened.st_size >= 0 && record.size == UInt64(opened.st_size)),
          record.exec_bits == nil || (opened.st_mode & 0o111 == mode_t(record.exec_bits!) && opened.st_mode & 0o7002 == 0) else {
        throw CocoaError(.fileReadCorruptFile)
    }
    guard fsync(fd) == 0 else { throw POSIXError(.init(rawValue: errno) ?? .EIO) }
    if record.directory { return nil }
    guard let node = record.node else { throw CocoaError(.fileReadCorruptFile) }
    return NativeStoredFile(node: node, version: record.version, size: UInt64(opened.st_size))
    }
}
private func coordinatedRead<T>(_ url: URL, _ read: (URL) throws -> T) throws -> T {
    let scoped = url.startAccessingSecurityScopedResource()
    defer { if scoped { url.stopAccessingSecurityScopedResource() } }
    var coordinationError: NSError?
    var result: Result<T, Error> = .failure(CocoaError(.fileReadUnknown))
    NSFileCoordinator().coordinate(readingItemAt: url, options: [], error: &coordinationError) { readable in
        result = Result { try read(readable) }
    }
    if let coordinationError { throw coordinationError }
    return try result.get()
}
// Deep app bundles contain many small files. Waiting for requestDownload, URL
// resolution, coordinated EOF and fsync for each one in serial adds every IPC
// and network round trip to Finder's Preparing phase. Keep eight native items
// in flight, enumerate a directory before scheduling its children, and serialize
// receipt publication after each completed file. Every byte remains owned by
// File Provider; this creates no second tree and never mutates the source.
struct NativeDownloadStep {
    var children: [Record] = []
    var stored: NativeStoredFile? = nil
}
func downloadReceivedTree(_ roots: [Record], parallelism: Int = 8,
                          visit: @escaping (Record) async throws -> NativeDownloadStep,
                          stored: (NativeStoredFile) throws -> Void) async throws {
    precondition(parallelism > 0)
    try await withThrowingTaskGroup(of: (NativeDownloadStep, Int).self) { group in
        var queue = roots.map { ($0, 0) }
        var cursor = 0
        var active = 0
        var seen = Set<String>()
        while cursor < queue.count || active > 0 {
            while cursor < queue.count && active < parallelism {
                let (record, depth) = queue[cursor]; cursor += 1
                guard depth <= 256, seen.count < 100_000, seen.insert(record.id).inserted else {
                    throw CocoaError(.fileReadTooLarge)
                }
                active += 1
                group.addTask {
                    try Task.checkCancellation()
                    return (try await visit(record), depth)
                }
            }
            guard let (step, depth) = try await group.next() else { break }
            active -= 1
            guard step.children.count <= 100_000 - queue.count else { throw CocoaError(.fileReadTooLarge) }
            queue.append(contentsOf: step.children.map { ($0, depth + 1) })
            if let receipt = step.stored { try stored(receipt) }
        }
    }
}

private func downloadNative(_ manager: NSFileProviderManager, _ record: Record) async throws {
    try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
        manager.requestDownloadForItem(withIdentifier: .init(record.id), requestedRange: nil) { error in
            if let error { continuation.resume(throwing: error) }
            else { continuation.resume() }
        }
    }
}
// This reads the native received file. A dataless file hydrates through the
// extension; all later readers use the same materialized system copy.
func readNative(_ url: URL, offset: UInt64, output: Bool, activity: NativeActivity, residentOnly: Bool = false) throws -> UInt64 {
    try coordinatedRead(url) { readable in
        let read = {
        if residentOnly {
            var metadata = stat()
            guard lstat(readable.path, &metadata) == 0, metadata.st_flags & UInt32(SF_DATALESS) == 0,
                  metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else { throw CocoaError(.fileReadUnknown) }
        }
        let fd = Darwin.open(readable.path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
        guard fd >= 0 else { throw CocoaError(.fileReadUnknown) }
        let input = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        defer { try? input.close() }
        try input.seek(toOffset: offset)
        var count = offset
        while let bytes = try input.read(upToCount: 1024 * 1024), !bytes.isEmpty {
            guard count <= UInt64.max - UInt64(bytes.count) else { throw CocoaError(.fileReadTooLarge) }
            count += UInt64(bytes.count)
            if output { try FileHandle.standardOutput.write(contentsOf: bytes) }
            activity.touch()
        }
        if !output || residentOnly {
            // A download-request ACK alone does not establish Stored. Wait for
            // coordinated EOF and flush the received file before its receipt.
            guard fsync(input.fileDescriptor) == 0 else { throw POSIXError(.init(rawValue: errno) ?? .EIO) }
            let attributes = try FileManager.default.attributesOfItem(atPath: readable.path)
            guard (attributes[.size] as? NSNumber)?.uint64Value == count else { throw CocoaError(.fileReadCorruptFile) }
        }
        activity.touch()
        return count
        }
        return try residentOnly ? withoutNativeHydration(read) : read()
    }
}
// User decision 2026-10-29: trust the native MOVE report like Explorer. Check
// only ownership/location and inode identity, never consumption, pin state or
// content. A target location and a replacement at the old path are independent.
private struct MoveCandidate {
    let record: Record
    let url: URL
    let identity: stat
}
func removeMovedNative(_ url: URL, record: Record, identity: stat, root: URL) throws {
    var coordinationError: NSError?
    var failure: Error?
    NSFileCoordinator().coordinate(writingItemAt: url, options: .forDeleting, error: &coordinationError) { current in
        do {
            var metadata = stat()
            guard current.standardizedFileURL == url.standardizedFileURL else { throw CocoaError(.fileWriteUnknown) }
            if lstat(current.path, &metadata) != 0 {
                if errno == ENOENT { return }
                throw POSIXError(.init(rawValue: errno) ?? .EIO)
            }
            guard metadata.st_dev == identity.st_dev, metadata.st_ino == identity.st_ino,
                  metadata.st_mode & S_IFMT == identity.st_mode & S_IFMT else { throw CocoaError(.fileWriteUnknown) }
            if record.directory {
                guard rmdir(current.path) == 0 else { throw POSIXError(.init(rawValue: errno) ?? .EIO) }
            } else {
                guard unlink(current.path) == 0 else { throw POSIXError(.init(rawValue: errno) ?? .EIO) }
            }
            cacheStorageChanged()
        } catch { failure = error }
    }
    if let coordinationError { throw coordinationError }
    if let failure { throw failure }
    try receivedDelete([record.id], in: root)
}
// A moved item may keep its provider identity inside the same domain. Do not
// follow that identity to the target's new location when cleaning this drag.
func receivedPublicationPath(_ record: Record, catalog: Catalog, batch: String) throws -> URL {
    let roots = catalog.records.filter { $0.parent == batch + "-root" }
    guard roots.count == catalog.urls.count else { throw CocoaError(.fileReadCorruptFile) }
    var current = record
    var suffix: [String] = []
    var seen = Set<String>()
    for _ in 0..<256 {
        guard seen.insert(current.id).inserted else { break }
        if let index = roots.firstIndex(where: { $0.id == current.id }) {
            return suffix.reversed().reduce(URL(fileURLWithPath: catalog.urls[index])) { $0.appendingPathComponent($1) }
        }
        guard !current.name.isEmpty, current.name != ".", current.name != "..", !current.name.contains("/"),
              let parent = catalog.records.first(where: { $0.id == current.parent }) else { break }
        suffix.append(current.name)
        current = parent
    }
    throw CocoaError(.fileReadCorruptFile)
}
private func retireMovedNative(_ batch: String, catalog: Catalog, manager: NSFileProviderManager,
                               root: URL, activity: NativeActivity) async throws {
    guard catalog.move_state == "pending" || catalog.move_state == "observed" else { throw CocoaError(.fileWriteUnknown) }
    let records = try catalogs(in: root).flatMap { $0.records }.filter { receivedKey($0.id) == batch }
    let originalRoots = Set(catalog.records.filter { $0.parent == batch + "-root" }.map(\.id))
    var included = originalRoots
    for _ in 0..<256 {
        let before = included.count
        for record in records where included.contains(record.parent) { included.insert(record.id) }
        if included.count == before { break }
    }
    var candidates: [MoveCandidate] = []
    for record in records where included.contains(record.id) {
        let url: URL
        do { url = try await manager.getUserVisibleURL(for: .init(record.id)) }
        catch {
            // A missing item is log-only retirement. Offline/provider failures
            // do not establish a move-out and leave the durable pending marker.
            guard (error as NSError).domain == NSFileProviderErrorDomain,
                  (error as NSError).code == NSFileProviderError.noSuchItem.rawValue else { throw error }
            try receivedDelete([record.id], in: root)
            continue
        }
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        let (identifier, domain) = try await NSFileProviderManager.identifierForUserVisibleFile(at: url)
        guard identifier.rawValue == record.id, domain.rawValue == providerDomain else { throw CocoaError(.fileWriteNoPermission) }
        // Optimized moves may retain the provider identity at a new target
        // location. Converge the old publication without deleting that target.
        guard try receivedPublicationPath(record, catalog: catalog, batch: batch).standardizedFileURL == url.standardizedFileURL else { continue }
        var identity = stat()
        if lstat(url.path, &identity) != 0 {
            guard errno == ENOENT else { throw POSIXError(.init(rawValue: errno) ?? .EIO) }
            try receivedDelete([record.id], in: root)
            continue
        }
        candidates.append(MoveCandidate(record: record, url: url, identity: identity))
        activity.touch()
    }
    // Children precede parents. Actual filesystem errors remain failures and
    // retry; a surviving unread/edited source copy is exactly what MOVE deletes.
    for candidate in candidates.sorted(by: { $0.url.pathComponents.count > $1.url.pathComponents.count }) {
        let scoped = candidate.url.startAccessingSecurityScopedResource()
        defer { if scoped { candidate.url.stopAccessingSecurityScopedResource() } }
        try removeMovedNative(candidate.url, record: candidate.record, identity: candidate.identity, root: root)
        activity.touch()
    }
    try await manager.signalEnumerator(for: .workingSet)
}
func nativeReceived(_ command: String, arguments: [String], manager: NSFileProviderManager,
                    activity: NativeActivity) async throws {
    guard let batch = arguments.first, !batch.isEmpty,
          batch.allSatisfy({ $0.isHexDigit || $0 == "-" }) else { throw CocoaError(.fileReadInvalidFileName) }
    let catalogURL = try container().appendingPathComponent(batch + ".json")
    func catalog() throws -> Catalog { try JSONDecoder().decode(Catalog.self, from: Data(contentsOf: catalogURL)) }
    if command == "received-move" {
        guard arguments.count == 1 else { throw CocoaError(.fileReadInvalidFileName) }
        try await retireMovedNative(batch, catalog: catalog(), manager: manager,
                                    root: catalogURL.deletingLastPathComponent(), activity: activity)
        return
    }
    if command == "received-link" {
        guard arguments.count == 2, let node = UInt64(arguments[1]),
              let record = try catalog().records.first(where: { $0.node == node }), let target = record.symlink_target else {
            throw CocoaError(.fileReadInvalidFileName)
        }
        let url = try await manager.getUserVisibleURL(for: .init(record.id))
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        _ = try syncResidentNative(url, record: record)
        try FileHandle.standardOutput.write(contentsOf: JSONEncoder().encode(target))
        return
    }
    if command == "received-read" || command == "received-stat" {
        guard arguments.count == (command == "received-stat" ? 2 : 3), let node = UInt64(arguments[1]),
              let record = try catalog().records.first(where: { $0.node == node && !$0.directory && $0.symlink_target == nil }) else {
            throw CocoaError(.fileReadInvalidFileName)
        }
        let url = try await manager.getUserVisibleURL(for: .init(record.id))
        if command != "received-stat" {
            guard let offset = UInt64(arguments[2]) else { throw CocoaError(.fileReadInvalidFileName) }
            _ = try readNative(url, offset: offset, output: true, activity: activity)
            guard let current = try catalog().records.first(where: { $0.id == record.id }) else { throw CocoaError(.fileReadCorruptFile) }
            try applyReceivedExecutableBits(url, record: current)
        } else {
            let values = try FileManager.default.attributesOfItem(atPath: url.path)
            guard let size = (values[.size] as? NSNumber)?.uint64Value else { throw CocoaError(.fileReadCorruptFile) }
            let modified = (values[.modificationDate] as? Date).map { Int64($0.timeIntervalSince1970 * 1000) }
            try FileHandle.standardOutput.write(contentsOf: JSONEncoder().encode(NativeFileMetadata(size: size, modified_ms: modified, exec_bits: (values[.posixPermissions] as? NSNumber).map { $0.uint16Value & 0o111 })))
        }
        return
    }
    guard command == "received-download", arguments.count == 1 else { throw CocoaError(.fileReadInvalidFileName) }
    let roots = try catalog().records.filter { $0.parent == batch + "-root" }
    try await downloadReceivedTree(roots, visit: { record in
        if record.symlink_target == nil { try await downloadNative(manager, record) }
        try Task.checkCancellation()
        let url = try await manager.getUserVisibleURL(for: .init(record.id))
        let step: NativeDownloadStep
        if record.directory {
            // requestDownload(directory) populates one level. Coordinated
            // readdir waits for it; children are scheduled only after completion.
            _ = try coordinatedRead(url) { try FileManager.default.contentsOfDirectory(at: $0, includingPropertiesForKeys: []) }
            step = NativeDownloadStep(children: try catalog().records.filter { $0.parent == record.id })
        } else if record.symlink_target != nil {
            guard let stored = try syncResidentNative(url, record: record) else { throw CocoaError(.fileReadCorruptFile) }
            step = NativeDownloadStep(stored: stored)
        } else {
            let size = try readNative(url, offset: 0, output: false, activity: activity)
            guard let current = try catalog().records.first(where: { $0.id == record.id }),
                  let node = current.node, current.size == size, current.version != 0 else { throw CocoaError(.fileReadCorruptFile) }
            try applyReceivedExecutableBits(url, record: current)
            _ = try syncResidentNative(url, record: current)
            step = NativeDownloadStep(stored: NativeStoredFile(node: node, version: current.version, size: size))
        }
        activity.touch()
        return step
    }, stored: { receipt in
        var line = try JSONEncoder().encode(receipt); line.append(10)
        try FileHandle.standardOutput.write(contentsOf: line)
    })
}

// Apply after coordinated EOF, using the selected record persisted by fetch.
// A metadata-only chmod racing hydration must not restore the older list's bits.
func applyReceivedExecutableBits(_ url: URL, record: Record) throws {
    guard record.exec_bits != nil else { return }
    try withoutNativeHydration {
        var metadata = stat()
        guard lstat(url.path, &metadata) == 0, metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
              metadata.st_flags & UInt32(SF_DATALESS) == 0 else { throw CocoaError(.fileReadUnknown) }
        let fd = Darwin.open(url.path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
        guard fd >= 0 else { throw CocoaError(.fileReadUnknown) }
        defer { close(fd) }
        try applyExecutableBits(record.exec_bits, descriptor: fd)
        guard fsync(fd) == 0 else { throw CocoaError(.fileWriteUnknown) }
    }
}
