import Foundation
import FileProvider
import UniformTypeIdentifiers
import CryptoKit
import Darwin
import OSLog

#if OINK_RELEASE
private let providerGroupKey = ReleaseIdentity.appGroupKey
let providerDisplayName = ReleaseIdentity.displayName
let providerDomain = ReleaseIdentity.domain
let providerLogSubsystem = ReleaseIdentity.subsystem
#else
private let providerGroupKey = "OinkAppGroup"
let providerDisplayName = "Oink"
let providerDomain = "oink-files"
let providerLogSubsystem = "com.hachiwii.oink.fileprovider"
#endif

private let approvedAppGroup = "893V37X2CW.com.hachiwii.oink.files"

private let anchorLog = Logger(subsystem: providerLogSubsystem, category: "enumeration")

// These records authorize local content operations. Unknown execution fields or
// schemas must fail closed instead of being silently omitted during recovery.
private struct LocalKey: CodingKey {
    let stringValue: String
    var intValue: Int? { nil }
    init?(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { return nil }
}
private func validateLocalKeys(_ decoder: Decoder, allowed: [String], schema: Bool = false) throws {
    let values = try decoder.container(keyedBy: LocalKey.self)
    guard values.allKeys.allSatisfy({ allowed.contains($0.stringValue) }) else {
        throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "Unknown local field"))
    }
    if schema {
        guard try values.decode(UInt8.self, forKey: LocalKey(stringValue: "schema")!) == 1 else {
            throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "Unsupported local schema"))
        }
    }
}

struct Record: Codable, Equatable {
    let id: String
    let parent: String
    let name: String
    let directory: Bool
    let size: UInt64?
    let modified: Int64?
    let node: UInt64?
    let version: UInt64
    let children: UInt64?
    // Present on private placeholder pool items.
    let hidden: Bool?
    init(id: String, parent: String, name: String, directory: Bool, size: UInt64?, modified: Int64?, node: UInt64?, version: UInt64 = 0, children: UInt64? = nil, hidden: Bool? = nil) {
        self.id = id; self.parent = parent; self.name = name; self.directory = directory
        self.size = size; self.modified = modified; self.node = node; self.version = version; self.children = children; self.hidden = hidden
    }
    // Keep the item identity stable while replacing the content/metadata version.
    // The backend has persisted the same metadata before sending this header.
    func updating(_ metadata: CurrentMetadata) -> Record {
        Record(id: id, parent: parent, name: name, directory: directory, size: metadata.size,
               modified: metadata.modified, node: node, version: metadata.version, children: children, hidden: hidden)
    }
    enum CodingKeys: String, CodingKey { case id, parent, name, directory, size, modified, node, version, children, hidden }
    init(from decoder: Decoder) throws {
        try validateLocalKeys(decoder, allowed: ["id", "parent", "name", "directory", "size", "modified", "node", "version", "children", "hidden"])
        let values = try decoder.container(keyedBy: CodingKeys.self)
        id = try values.decode(String.self, forKey: .id)
        parent = try values.decode(String.self, forKey: .parent)
        name = try values.decode(String.self, forKey: .name)
        directory = try values.decode(Bool.self, forKey: .directory)
        size = try values.decodeIfPresent(UInt64.self, forKey: .size)
        modified = try values.decodeIfPresent(Int64.self, forKey: .modified)
        node = try values.decodeIfPresent(UInt64.self, forKey: .node)
        version = try values.decode(UInt64.self, forKey: .version)
        children = try values.decodeIfPresent(UInt64.self, forKey: .children)
        hidden = try values.decodeIfPresent(Bool.self, forKey: .hidden)
    }
}
struct CurrentMetadata: Codable {
    let schema: UInt8
    let size: UInt64
    let modified: Int64?
    let version: UInt64
    enum CodingKeys: String, CodingKey { case schema, size, modified, version }
    init(from decoder: Decoder) throws {
        try validateLocalKeys(decoder, allowed: ["schema", "size", "modified", "version"], schema: true)
        let values = try decoder.container(keyedBy: CodingKeys.self)
        schema = try values.decode(UInt8.self, forKey: .schema)
        size = try values.decode(UInt64.self, forKey: .size)
        modified = try values.decodeIfPresent(Int64.self, forKey: .modified)
        version = try values.decode(UInt64.self, forKey: .version)
    }
}

func downloadProgress() -> Progress {
    let progress = Progress(totalUnitCount: -1)
    // fetchContents returns this object to File Provider. Declaring byte units
    // and the operation lets its standard UI describe the download while the
    // completion URL must still wait for this file's EOF. Finder owns the copy
    // dialog and may keep its separate "Preparing" label until materialization.
    progress.kind = .file
    progress.fileOperationKind = .downloading
    progress.fileTotalCount = 1
    progress.fileCompletedCount = 0
    return progress
}
struct Catalog: Codable {
    let schema: UInt8
    let token: String
    let port: UInt16
    let records: [Record]
    let batch: [UInt8]?
    let urls: [String]
    init(token: String, port: UInt16, records: [Record], batch: [UInt8]? = nil, urls: [String] = []) {
        self.schema = 1
        self.token = token; self.port = port; self.records = records; self.batch = batch; self.urls = urls
    }
    enum CodingKeys: String, CodingKey { case schema, token, port, records, batch, urls }
    init(from decoder: Decoder) throws {
        try validateLocalKeys(decoder, allowed: ["schema", "token", "port", "records", "batch", "urls"], schema: true)
        let values = try decoder.container(keyedBy: CodingKeys.self)
        schema = try values.decode(UInt8.self, forKey: .schema)
        token = try values.decode(String.self, forKey: .token)
        port = try values.decode(UInt16.self, forKey: .port)
        records = try values.decode([Record].self, forKey: .records)
        batch = try values.decodeIfPresent([UInt8].self, forKey: .batch)
        urls = try values.decode([String].self, forKey: .urls)
    }
}
struct DirectoryCursor: Codable, Equatable {
    let version: UInt64
    let offset: UInt32
}
struct DirectoryListing: Codable {
    let records: [Record]
    let next: DirectoryCursor?
    let version: UInt64
}
struct DirectoryPageToken: Codable {
    let directory: String
    let cursor: DirectoryCursor
}
struct BridgeRequest: Encodable {
    let operation: String
    let token: String
    let batch: String
    let node: UInt64
    let cursor: DirectoryCursor?
}
func bridgeRequest(_ operation: String, catalog: Catalog, record: Record, cursor: DirectoryCursor? = nil) throws -> Data {
    guard !isPreparedPlaceholder(record), let node = record.node, let batch = record.id.split(separator: "-").first else { throw NSFileProviderError(.noSuchItem) }
    var bytes = try JSONEncoder().encode(BridgeRequest(operation: operation, token: catalog.token, batch: String(batch), node: node, cursor: cursor))
    bytes.append(10)
    return bytes
}
func listDirectory(catalog: Catalog, record: Record, cursor: DirectoryCursor?) throws -> DirectoryListing {
    let fd = socket(AF_INET, SOCK_STREAM, 0)
    guard fd >= 0 else { throw NSFileProviderError(.serverUnreachable) }
    defer { close(fd) }
    var timeout = timeval(tv_sec: 310, tv_usec: 0)
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout.size(ofValue: timeout)))
    var noSigPipe: Int32 = 1
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout.size(ofValue: noSigPipe)))
    var address = sockaddr_in()
    address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
    address.sin_family = sa_family_t(AF_INET)
    address.sin_port = catalog.port.bigEndian
    address.sin_addr.s_addr = inet_addr("127.0.0.1")
    let connected = withUnsafePointer(to: &address) { p in p.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) } }
    guard connected == 0 else { throw NSFileProviderError(.serverUnreachable) }
    let request = try bridgeRequest("list", catalog: catalog, record: record, cursor: cursor)
    try request.withUnsafeBytes { bytes in
        var offset = 0
        while offset < bytes.count {
            let n = Darwin.write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
            guard n > 0 else { throw NSFileProviderError(.serverUnreachable) }
            offset += n
        }
    }
    func readExactly(_ count: Int) throws -> Data {
        var data = Data(count: count)
        try data.withUnsafeMutableBytes { bytes in
            var offset = 0
            while offset < count {
                let n = Darwin.read(fd, bytes.baseAddress!.advanced(by: offset), count - offset)
                if n < 0 && errno == EINTR { continue }
                guard n > 0 else { throw NSFileProviderError(.serverUnreachable) }
                offset += n
            }
        }
        return data
    }
    let length = try readExactly(4).reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
    if length == UInt32.max {
        switch try readExactly(1).first! {
        case 1: throw NSFileProviderError(.serverUnreachable)
        case 2: throw NSFileProviderError(.noSuchItem)
        case 3: throw NSFileProviderError(.versionNoLongerAvailable)
        case 4: throw CocoaError(.userCancelled)
        default: throw NSFileProviderError(.cannotSynchronize)
        }
    }
    guard length <= 4 * 1024 * 1024 else { throw NSFileProviderError(.cannotSynchronize) }
    let result = try JSONDecoder().decode(DirectoryListing.self, from: readExactly(Int(length)))
    guard result.records.count <= 1024,
          result.records.allSatisfy({ $0.parent == record.id && $0.node != nil }),
          Set(result.records.map { $0.id }).count == result.records.count,
          cursor == nil || cursor!.version == result.version else { throw NSFileProviderError(.versionNoLongerAvailable) }
    if let next = result.next {
        guard next.version == result.version, !result.records.isEmpty,
              UInt64(next.offset) == UInt64(cursor?.offset ?? 0) + UInt64(result.records.count) else { throw NSFileProviderError(.cannotSynchronize) }
    }
    return result
}
func container() throws -> URL {
    guard let group = Bundle.main.object(forInfoDictionaryKey: providerGroupKey) as? String, group == approvedAppGroup,
          let url = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: group) else {
        throw NSError(domain: "OinkFileProvider", code: 1, userInfo: [NSLocalizedDescriptionKey: "App Group unavailable"])
    }
    let result = url.appendingPathComponent("FileProvider", isDirectory: true)
    try FileManager.default.createDirectory(at: result, withIntermediateDirectories: true)
    return result
}
func catalogs() throws -> [Catalog] { try catalogs(in: container()) }
func catalogs(in root: URL) throws -> [Catalog] {
    return try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
        .filter { $0.pathExtension == "json" }
        .map { try JSONDecoder().decode(Catalog.self, from: Data(contentsOf: $0)) }
}
final class Item: NSObject, NSFileProviderItem {
    let record: Record
    init(_ record: Record) { self.record = record }
    var itemIdentifier: NSFileProviderItemIdentifier { .init(record.id) }
    var parentItemIdentifier: NSFileProviderItemIdentifier { .init(record.parent) }
    var filename: String { record.name }
    var contentType: UTType { record.directory ? .folder : (UTType(filenameExtension: (record.name as NSString).pathExtension) ?? .data) }
    var documentSize: NSNumber? { record.size.map { NSNumber(value: $0) } }
    var childItemCount: NSNumber? { record.children.map { NSNumber(value: $0) } }
    var contentModificationDate: Date? { record.modified.map { Date(timeIntervalSince1970: Double($0) / 1000) } }
    // These snapshots export ordinary copies. POSIX userWritable alone still produces uchg
    // on macOS 14 and 26 when capabilities only allow reading; Finder carries it to copies.
    // Declare ordinary operations too, including rename/move, which an immutable item forbids.
    // Reading/writing alias content enumeration/adding children on directories. The extension
    // still rejects every mutation callback: these declarations never authorize remote writes.
    var capabilities: NSFileProviderItemCapabilities {
        [.allowsReading, .allowsWriting, .allowsRenaming, .allowsReparenting, .allowsTrashing, .allowsDeleting]
    }
    var fileSystemFlags: NSFileProviderFileSystemFlags {
        var flags: NSFileProviderFileSystemFlags = record.directory ? [.userReadable, .userWritable, .userExecutable] : [.userReadable, .userWritable]
        if record.hidden == true { flags.insert(.hidden) }
        return flags
    }
    var itemVersion: NSFileProviderItemVersion {
        let size = record.size.map(String.init) ?? "unknown"
        let modified = record.modified.map(String.init) ?? "unknown"
        let content = "\(record.version)-\(size)-\(modified)"
        var metadata = "\(content)-\(record.children.map(String.init) ?? "unknown")"
        // A reused provider identity must signal the rename and clear UF_HIDDEN
        // even when the real directory still has unknown size and version 1.
        // Generic -> final is metadata; never convert a file identity to folder.
        if record.id.contains("-slot-") || record.hidden != nil {
            metadata += "-\(record.name)-\(record.hidden == true)"
        }
        return .init(contentVersion: Data(content.utf8), metadataVersion: Data(metadata.utf8))
    }
}
func rootItem() -> Item {
    Item(Record(id: NSFileProviderItemIdentifier.rootContainer.rawValue, parent: NSFileProviderItemIdentifier.rootContainer.rawValue, name: providerDisplayName, directory: true, size: 0, modified: nil, node: nil))
}

// Persist metadata-only snapshots across extension restarts. Anchors are bounded (72 bytes),
// deterministic, and contain neither the bridge capability nor user file names.
struct CatalogSnapshot {
    private static let lock = NSLock()
    let records: [Record]
    let anchor: NSFileProviderSyncAnchor

    static func capture(in root: URL) throws -> CatalogSnapshot {
        lock.lock(); defer { lock.unlock() }
        let records = try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "json" }
            .flatMap { try JSONDecoder().decode(Catalog.self, from: Data(contentsOf: $0)).records }
            .sorted { $0.id < $1.id }
        guard Set(records.map(\.id)).count == records.count else { throw CocoaError(.fileReadCorruptFile) }
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let data = try encoder.encode(records)
        // The anchor prefix versions this metadata snapshot format independently
        // of the network protocol.
        let key = "oink-v3-" + SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        let directory = root.appendingPathComponent("anchors", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let file = directory.appendingPathComponent(key)
        // A cache filename alone does not make an anchor usable. In particular,
        // after expiry currentSyncAnchor must not reissue a corrupt existing file
        // with the same content-addressed name. Verify against current bytes;
        // this reconstructs our cache, never reads/converts an older schema.
        let existing = FileManager.default.fileExists(atPath: file.path) ? try Data(contentsOf: file) : nil
        if existing != data {
            try data.write(to: file, options: .atomic)
            if existing != nil { anchorLog.notice("operation=anchor-repair count=1") }
            // A genuinely old anchor may expire. Never expire the snapshot just issued.
            let history = try FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: [.contentModificationDateKey])
                .filter { $0 != file }
                .sorted {
                    let lhs = (try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
                    let rhs = (try? $1.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
                    return lhs > rhs
                }
            for old in history.dropFirst(127) { try? FileManager.default.removeItem(at: old) }
        }
        return CatalogSnapshot(records: records, anchor: NSFileProviderSyncAnchor(Data(key.utf8)))
    }

    private static func discardUnusableSnapshot(_ file: URL) {
        do {
            try FileManager.default.removeItem(at: file)
            anchorLog.notice("operation=anchor-discard count=1 result=0")
        } catch {
            // Another extension instance may have discarded it already. Cleanup
            // failure must not turn expiry into a different transient daemon error.
            let code = (error as NSError).code
            anchorLog.notice("operation=anchor-discard count=0 result=\(code)")
        }
    }

    static func load(_ anchor: NSFileProviderSyncAnchor, in root: URL) throws -> CatalogSnapshot {
        lock.lock(); defer { lock.unlock() }
        // The expired-anchor observer contract starts over with a nil anchor.
        // Accept its empty-data representation, but never treat an arbitrary
        // unknown nonempty token as an empty namespace.
        if anchor.rawValue.isEmpty { return CatalogSnapshot(records: [], anchor: anchor) }
        guard let key = String(data: anchor.rawValue, encoding: .utf8), key.hasPrefix("oink-v3-"),
              key.count == 72, key.dropFirst(8).allSatisfy({ $0.isHexDigit }) else {
            throw NSFileProviderError(.syncAnchorExpired)
        }
        let file = root.appendingPathComponent("anchors").appendingPathComponent(key)
        guard FileManager.default.fileExists(atPath: file.path) else { throw NSFileProviderError(.syncAnchorExpired) }
        let data = try Data(contentsOf: file)
        let records: [Record]
        do {
            records = try JSONDecoder().decode([Record].self, from: data)
        } catch {
            // Snapshot loss/decoding failure is generic expiry, never a schema
            // conversion or an empty success. Discard only our unreadable cache;
            // item identities and the live namespace remain untouched.
            discardUnusableSnapshot(file)
            throw NSFileProviderError(.syncAnchorExpired)
        }
        guard Set(records.map(\.id)).count == records.count else {
            discardUnusableSnapshot(file)
            throw NSFileProviderError(.syncAnchorExpired)
        }
        return CatalogSnapshot(records: records, anchor: anchor)
    }

    func scoped(to identifier: NSFileProviderItemIdentifier) -> [Record] {
        if identifier == .workingSet {
            // Hashing uses identifier order, but the system must learn each directory before
            // its descendants. Otherwise URL publication waits for parent reconciliation.
            let ids = Set(records.map { $0.id })
            let children = Dictionary(grouping: records, by: { $0.parent })
            var pending = Array(records.filter { !ids.contains($0.parent) }.reversed())
            var ordered: [Record] = []
            var visited = Set<String>()
            while let record = pending.popLast() {
                guard visited.insert(record.id).inserted else { continue }
                ordered.append(record)
                pending.append(contentsOf: (children[record.id] ?? []).reversed())
            }
            return ordered
        }
        if let item = records.first(where: { $0.id == identifier.rawValue }), !item.directory { return [item] }
        return records.filter { $0.parent == identifier.rawValue }
    }

    func changes(since old: CatalogSnapshot, in identifier: NSFileProviderItemIdentifier) -> (updated: [Record], deleted: [NSFileProviderItemIdentifier]) {
        let previous = Dictionary(uniqueKeysWithValues: old.scoped(to: identifier).map { ($0.id, $0) })
        let current = scoped(to: identifier)
        let ids = Set(current.map { $0.id })
        return (current.filter { previous[$0.id] != $0 }, old.scoped(to: identifier).reversed().filter { !ids.contains($0.id) }.map { NSFileProviderItemIdentifier($0.id) })
    }
}

// Resolve metadata URLs concurrently, retaining original manifest order. A serial lookup adds
// the OS namespace synchronization latency once per item (and easily exceeds the drag budget).
func resolveItemURLs(_ records: [Record], resolved: (Record) -> Void = { _ in }, firstChild: () -> Void = {}, resolve: @escaping (Record) async throws -> String) async throws -> [String] {
    try await withThrowingTaskGroup(of: (Int, String).self) { group in
        var urls = Array(repeating: "", count: records.count)
        var next = 0
        var observedChild = false
        func enqueue(_ index: Int) {
            group.addTask { (index, try await resolve(records[index])) }
        }
        while next < min(16, records.count) { enqueue(next); next += 1 }
        while let (index, url) = try await group.next() {
            urls[index] = url
            resolved(records[index])
            if records[index].node != nil && !observedChild { observedChild = true; firstChild() }
            if next < records.count { enqueue(next); next += 1 }
        }
        return urls
    }
}

// Request-owned staging lives in the App Group so accounting never has to traverse
// the extension's arbitrary sandbox temporary directory. The system consumes the
// successful URL; handed-off files are retained until it does so.
func contentTemporaryDirectory() throws -> URL {
    let root = try container().appendingPathComponent("temporary", isDirectory: true)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    return root
}

final class MaterializedObserver: NSObject, NSFileProviderEnumerationObserver {
    let enumerator: NSFileProviderEnumerator
    var items: [NSFileProviderItem] = []
    var completion: ((Result<[NSFileProviderItem], Error>) -> Void)?
    init(_ enumerator: NSFileProviderEnumerator) { self.enumerator = enumerator }
    func didEnumerate(_ items: [NSFileProviderItem]) { self.items.append(contentsOf: items) }
    func finishEnumerating(upTo nextPage: NSFileProviderPage?) {
        if let nextPage { enumerator.enumerateItems(for: self, startingAt: nextPage) }
        else { completion?(.success(items)); completion = nil }
    }
    func finishEnumeratingWithError(_ error: Error) { completion?(.failure(error)); completion = nil }
    func read() async throws -> [NSFileProviderItem] {
        try await withCheckedThrowingContinuation { continuation in
            completion = { continuation.resume(with: $0) }
            enumerator.enumerateItems(for: self, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        }
    }
}


/// Serializes file creation/locking with manual clear across the extension and helper.
final class TemporaryRootLease {
    let fd: Int32
    init(_ root: URL) throws {
        fd = Darwin.open(root.path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW)
        guard fd >= 0 else { throw CocoaError(.fileReadUnknown) }
        guard flock(fd, LOCK_EX) == 0 else { close(fd); throw CocoaError(.fileReadUnknown) }
    }
    deinit { close(fd) }
}
struct TemporaryCacheResult { var bytes: UInt64 = 0; var kept: UInt64 = 0; var removed: UInt64 = 0; var incomplete = false }
func inspectTemporary(in root: URL, clear: Bool, evicted: Set<String>) throws -> TemporaryCacheResult {
    let lease = try TemporaryRootLease(root)
    defer { withExtendedLifetime(lease) {} }
    var result = TemporaryCacheResult()
    for url in try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil) {
        var metadata = stat()
        guard lstat(url.path, &metadata) == 0, metadata.st_mode & S_IFMT == S_IFREG else { result.incomplete = true; continue }
        let size = UInt64(max(0, metadata.st_blocks)) * 512
        let fd = Darwin.open(url.path, O_RDONLY | O_NOFOLLOW)
        defer { if fd >= 0 { close(fd) } }
        // A returned URL stays under the system's control until the corresponding
        // materialized item has been successfully evicted. Busy writers are skipped.
        let handoff = url.lastPathComponent.hasPrefix("handoff-")
        let busy = (handoff && !evicted.contains(where: { url.lastPathComponent.hasPrefix("handoff-" + $0 + "-") })) || fd < 0 || flock(fd, LOCK_EX | LOCK_NB) != 0
        if clear && !busy {
            precondition(url.deletingLastPathComponent().standardizedFileURL == root.standardizedFileURL)
            do { try FileManager.default.removeItem(at: url); result.removed += size; continue } catch {}
        }
        result.bytes += size
        if clear || busy { result.kept += size }
    }
    return result
}

struct CacheProtection: Decodable { let items: Set<String>; let paths: [String] }
func sourceProtects(_ url: URL, paths: [String]) -> Bool {
    let path = url.standardizedFileURL.path
    return paths.contains { source in
        let p = URL(fileURLWithPath: source).standardizedFileURL.path
        return path == p || path.hasPrefix(p + "/") || p.hasPrefix(path + "/")
    }
}


// Record the first callback reporting every published top-level child. This is
// extension observation, not proof that fileproviderd has applied the update.
// O_EXCL makes repeated enumerations harmless; one-shot batch IDs prevent reuse.
func recordChildrenEnumerated(_ records: [Record], in root: URL) {
    let reported = Set(records.map { $0.id })
    let parents = Set(records.filter { $0.node != nil }.map { $0.parent })
    for parent in parents where parent.hasSuffix("-root") {
        let key = String(parent.dropLast(5))
        let marker = root.appendingPathComponent(key + ".enumerated")
        // Idle working-set enumerations must not repeatedly decode every old
        // completed catalog just for timing. O_EXCL below still arbitrates races.
        guard key.count == 64, key.allSatisfy({ $0.isHexDigit }),
              !FileManager.default.fileExists(atPath: marker.path),
              let data = try? Data(contentsOf: root.appendingPathComponent(key + ".json")),
              let catalog = try? JSONDecoder().decode(Catalog.self, from: data) else { continue }
        let children = catalog.records.filter { $0.parent == parent }
        guard !children.isEmpty, children.allSatisfy({ reported.contains($0.id) }) else { continue }
        // Slots keep their identifiers across activation. A late generic page
        // is not evidence that the final names/metadata have been reported.
        if children.contains(where: { $0.id.contains("-slot-") }) {
            guard !children.contains(where: isPreparedPlaceholder), children.allSatisfy({ records.contains($0) }) else { continue }
        }
        let fd = open(marker.path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, mode_t(0o600))
        guard fd >= 0 else { continue }
        let bytes = Array(String(clock_gettime_nsec_np(CLOCK_UPTIME_RAW)).utf8)
        _ = bytes.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
        close(fd)
    }
}

// Keep the readiness rule in one place so tests can delay/fail each public-API
// boundary without registering a domain or accessing the user's provider files.
@discardableResult
func prepareSpareDirectory(resolve: () async throws -> URL,
                           download: () async throws -> Void,
                           materialized: (URL) throws -> Bool) async throws -> URL {
    let url = try await resolve()
    try await download()
    guard try materialized(url) else { throw CocoaError(.fileReadUnknown) }
    return url
}

struct VisibleMetadata {
    let directory: Bool
    let size: UInt64
    let hidden: Bool
}
func finalItemMatches(_ record: Record, url: URL, metadata: VisibleMetadata?) -> Bool {
    guard let metadata, url.lastPathComponent == record.name,
          metadata.directory == record.directory, metadata.hidden == (record.hidden == true) else { return false }
    return record.directory || record.size == nil || record.size == metadata.size
}

func isPreparedPlaceholder(_ record: Record) -> Bool { record.hidden == true && record.node != nil }
func visibleMetadata(_ url: URL) -> VisibleMetadata? {
    let scoped = url.startAccessingSecurityScopedResource()
    defer { if scoped { url.stopAccessingSecurityScopedResource() } }
    var metadata = stat()
    guard lstat(url.path, &metadata) == 0 else { return nil }
    let kind = metadata.st_mode & mode_t(S_IFMT)
    guard kind == mode_t(S_IFREG) || kind == mode_t(S_IFDIR) else { return nil }
    return VisibleMetadata(directory: kind == mode_t(S_IFDIR), size: UInt64(max(0, metadata.st_size)),
                           hidden: metadata.st_flags & UInt32(UF_HIDDEN) != 0)
}
func resolveFinalItemURL(_ record: Record, resolve: () async throws -> URL,
                         inspect: (URL) -> VisibleMetadata? = visibleMetadata,
                         pause: () async throws -> Void = { try await Task.sleep(nanoseconds: 10_000_000) }) async throws -> URL {
    // macOS 14/26 return a cached generic URL immediately after a WS rename.
    // Waiting only for the first callback hands native readers the wrong name,
    // zero size and sometimes an inode that the daemon is about to replace.
    // The manager watchdog and owning Rust publication bound/cancel this loop.
    while true {
        let url = try await resolve()
        if finalItemMatches(record, url: url, metadata: inspect(url)) { return url }
        try await pause()
    }
}
struct PlaceholderLedger: Codable {
    let schema: UInt8
    let parent: String?
    let urls: [String]
    enum CodingKeys: String, CodingKey { case schema, parent, urls }
    init(from decoder: Decoder) throws {
        try validateLocalKeys(decoder, allowed: ["schema", "parent", "urls"], schema: true)
        let values = try decoder.container(keyedBy: CodingKeys.self)
        schema = try values.decode(UInt8.self, forKey: .schema)
        parent = try values.decodeIfPresent(String.self, forKey: .parent)
        urls = try values.decode([String].self, forKey: .urls)
    }
}

// A live replicated extension must be suspended before its bundle is replaced.
// Killing it during a rename left macOS 26's change stream throttled after the
// new extension launched: new IDs resolved, but existing slots never renamed.
// Use Apple's update-specific temporary disconnect, then explicitly reconnect
// after replacement or rollback. Neither command removes/reimports the domain.
enum ProviderUpdateAction: String {
    case prepare = "prepare-update"
    case finish = "finish-update"

    func perform(disconnect: (NSFileProviderManager.DisconnectionOptions) async throws -> Void,
                 reconnect: () async throws -> Void) async throws {
        switch self {
        case .prepare: try await disconnect([.temporary])
        case .finish: try await reconnect()
        }
    }
}

// A manual bundle replacement already killed the old extension. On macOS 26,
// new URL resolution and working-set signals did not clear the retained failing
// event stream; a temporary disconnect followed by reconnect with the intact
// registered bundle did. Preserve the domain and the user's disabled state.
// Reconnect an enabled domain left disconnected by an interrupted attempt.
// This recovery shape still needs signed manual-update acceptance on macOS 14/26.
func recoverProviderLaunch(hidden: Bool, enabled: Bool, disconnected: Bool,
                           disconnect: (NSFileProviderManager.DisconnectionOptions) async throws -> Void,
                           reconnect: () async throws -> Void) async throws {
    guard hidden else { throw CocoaError(.fileReadUnknown) }
    guard enabled else { return }
    if !disconnected { try await disconnect([.temporary]) }
    try await reconnect()
}

enum ProviderRemovalError: Error {
    case preservedData
    case domainsRemain
}

enum ProviderRemovalAction: String {
    case remove = "remove-domain"

    func perform(domains: [NSFileProviderDomain],
                 remove: (NSFileProviderDomain, NSFileProviderManager.DomainRemovalMode) async throws -> URL?,
                 remaining: () async throws -> [NSFileProviderDomain]) async throws -> String {
        guard !domains.isEmpty else { return "absent" }
        var failure: Error?
        // The list is scoped to this extension, not every provider on the Mac.
        // Remove every Oink domain regardless of its ID, visibility or connection.
        for domain in domains {
            do {
                // A full uninstall discards Oink-managed snapshots/prepared slots;
                // this domain does not accept user-authored data. Preservation
                // modes intentionally leave local files behind and are unsuitable.
                if try await remove(domain, .removeAll) != nil {
                    throw ProviderRemovalError.preservedData
                }
            } catch { if failure == nil { failure = error } }
        }
        if let failure { throw failure }
        guard try await remaining().isEmpty else { throw ProviderRemovalError.domainsRemain }
        return "removed"
    }
}
