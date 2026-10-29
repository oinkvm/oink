import Foundation
// Descriptions/userInfo may contain user paths or bridge tokens. The host only
// accepts this code-only record, never arbitrary helper stderr.
func providerFailureDiagnostic(_ error: Error) -> String {
    let domains: Set<String> = ["NSCocoaErrorDomain", "NSPOSIXErrorDomain", "NSOSStatusErrorDomain",
        "NSMachErrorDomain", "NSFileProviderErrorDomain", "FPErrorDomain", "OinkFileProvider"]
    var errors: [[String: Any]] = []
    var current: NSError? = error as NSError
    while let error = current, errors.count < 8 {
        errors.append(["domain": domains.contains(error.domain) ? error.domain : "other", "code": error.code])
        current = error.userInfo[NSUnderlyingErrorKey] as? NSError
    }
    // All values are strings/integers assembled above; JSON serialization cannot
    // fail because of data supplied in NSError.userInfo.
    let data = try! JSONSerialization.data(withJSONObject: ["errors": errors], options: [.sortedKeys])
    return "OINK_FP_ERROR " + String(decoding: data, as: UTF8.self)
}
import FileProvider
import UniformTypeIdentifiers
import CryptoKit
import Darwin
import notify
import OSLog

#if OINK_RELEASE
private let providerGroupKey = ReleaseIdentity.appGroupKey
let providerDisplayName = ReleaseIdentity.displayName
private let providerDomainBase = ReleaseIdentity.domain
let providerLogSubsystem = ReleaseIdentity.subsystem
#else
private let providerGroupKey = "OinkAppGroup"
let providerDisplayName = "Oink"
private let providerDomainBase = "oink-files"
let providerLogSubsystem = "com.hachiwii.oink.fileprovider"
#endif
let providerDomain = providerDomainBase

private let approvedAppGroup = "893V37X2CW.com.hachiwii.oink.files"

// The Rust owner watcher subscribes before accounting. One wake reconciles the
// whole root, so coalescing is safe. FSEvents can omit a just-notified modify
// under load (see storage_watch.rs); supported commits must publish explicitly.
func cacheStorageChanged() {
    let status = notify_post("com.hachiwii.oink.cache-usage.changed")
    if status != NOTIFY_STATUS_OK { anchorLog.error("cache owner notification failed status=\(status)") }
    #if DEBUG
    CacheMutationTestProbe.record()
    #endif
}
#if DEBUG
enum CacheMutationTestProbe {
    private static let lock = NSLock()
    private static var value = 0
    static func record() { lock.lock(); value += 1; lock.unlock() }
    static var count: Int { lock.lock(); defer { lock.unlock() }; return value }
}
func assertCacheMutation<T>(_ mutation: () throws -> T) rethrows -> T {
    let before = CacheMutationTestProbe.count
    let result = try mutation()
    precondition(CacheMutationTestProbe.count > before, "owner returned without cache invalidation")
    return result
}
#endif

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
    let symlink_target: String?
    let exec_bits: UInt16?
    let size: UInt64?
    let modified: Int64?
    let created: Int64?
    let node: UInt64?
    let version: UInt64
    let children: UInt64?
    // Present on private placeholder pool items.
    let hidden: Bool?
    let keep_local: Bool?
    init(id: String, parent: String, name: String, directory: Bool, size: UInt64?, modified: Int64?, node: UInt64?, version: UInt64 = 0, children: UInt64? = nil, hidden: Bool? = nil, created: Int64? = nil, keep_local: Bool? = nil, symlink_target: String? = nil, exec_bits: UInt16? = nil) {
        self.id = id; self.parent = parent; self.name = name; self.directory = directory
        self.size = size; self.modified = modified; self.node = node; self.version = version; self.children = children; self.hidden = hidden
        self.created = created; self.keep_local = keep_local; self.symlink_target = symlink_target; self.exec_bits = exec_bits
    }
    // Keep the item identity stable while replacing the content/metadata version.
    // The backend has persisted the same metadata before sending this header.
    func updating(_ metadata: CurrentMetadata) -> Record {
        Record(id: id, parent: parent, name: name, directory: directory, size: metadata.size,
               modified: metadata.modified, node: node, version: metadata.version, children: children, hidden: hidden, created: created, keep_local: keep_local, symlink_target: symlink_target, exec_bits: metadata.exec_bits)
    }
    func withCreationDate(_ fallback: Int64?) -> Record {
        Record(id: id, parent: parent, name: name, directory: directory, size: size, modified: modified,
               node: node, version: version, children: children, hidden: hidden, created: created ?? fallback, keep_local: keep_local, symlink_target: symlink_target, exec_bits: exec_bits)
    }
    func retainingLocal(_ retain: Bool) -> Record {
        Record(id: id, parent: parent, name: name, directory: directory, size: size, modified: modified,
               node: node, version: version, children: children, hidden: hidden, created: created,
               keep_local: retain || keep_local == true ? true : nil, symlink_target: symlink_target, exec_bits: exec_bits)
    }
    enum CodingKeys: String, CodingKey { case id, parent, name, directory, size, modified, created, node, version, children, hidden, keep_local, symlink_target, exec_bits }
    init(from decoder: Decoder) throws {
        try validateLocalKeys(decoder, allowed: ["id", "parent", "name", "directory", "size", "modified", "created", "node", "version", "children", "hidden", "keep_local", "symlink_target", "exec_bits"])
        let values = try decoder.container(keyedBy: CodingKeys.self)
        id = try values.decode(String.self, forKey: .id)
        parent = try values.decode(String.self, forKey: .parent)
        name = try values.decode(String.self, forKey: .name)
        directory = try values.decode(Bool.self, forKey: .directory)
        size = try values.decodeIfPresent(UInt64.self, forKey: .size)
        modified = try values.decodeIfPresent(Int64.self, forKey: .modified)
        created = try values.decodeIfPresent(Int64.self, forKey: .created)
        node = try values.decodeIfPresent(UInt64.self, forKey: .node)
        version = try values.decode(UInt64.self, forKey: .version)
        children = try values.decodeIfPresent(UInt64.self, forKey: .children)
        hidden = try values.decodeIfPresent(Bool.self, forKey: .hidden)
        keep_local = try values.decodeIfPresent(Bool.self, forKey: .keep_local)
        symlink_target = try values.decodeIfPresent(String.self, forKey: .symlink_target)
        exec_bits = try values.decodeIfPresent(UInt16.self, forKey: .exec_bits)
        if let bits = exec_bits { guard !directory, symlink_target == nil, bits & ~0o111 == 0 else { throw CocoaError(.fileReadCorruptFile) } }
        if let target = symlink_target {
            guard !directory, node != nil, safeSymlinkTarget(target, parentDepth: 256), size == UInt64(target.utf8.count) else {
                throw CocoaError(.fileReadCorruptFile)
            }
        }
    }
}
struct CurrentMetadata: Codable {
    let schema: UInt8
    let size: UInt64
    let modified: Int64?
    let version: UInt64
    let exec_bits: UInt16?
    enum CodingKeys: String, CodingKey { case schema, size, modified, version, exec_bits }
    init(from decoder: Decoder) throws {
        try validateLocalKeys(decoder, allowed: ["schema", "size", "modified", "version", "exec_bits"], schema: true)
        let values = try decoder.container(keyedBy: CodingKeys.self)
        schema = try values.decode(UInt8.self, forKey: .schema)
        size = try values.decode(UInt64.self, forKey: .size)
        modified = try values.decodeIfPresent(Int64.self, forKey: .modified)
        version = try values.decode(UInt64.self, forKey: .version)
        exec_bits = try values.decodeIfPresent(UInt16.self, forKey: .exec_bits)
        if let bits = exec_bits, bits & ~0o111 != 0 { throw CocoaError(.fileReadCorruptFile) }
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
    let urls: [String]
    let received_at: Double?
    let move_state: String?
    var receivedMilliseconds: Int64? {
        guard let value = received_at, value.isFinite, value >= 0, value < Double(Int64.max) else { return nil }
        return Int64(value)
    }
    init(token: String, port: UInt16, records: [Record], urls: [String] = [], received_at: Double? = nil, move_state: String? = nil) {
        self.schema = 1
        self.token = token; self.port = port; self.records = records; self.urls = urls; self.received_at = received_at; self.move_state = move_state
    }
    enum CodingKeys: String, CodingKey { case schema, token, port, records, urls, received_at, move_state }
    init(from decoder: Decoder) throws {
        try validateLocalKeys(decoder, allowed: ["schema", "token", "port", "records", "source", "settlement", "arrived", "failed", "urls", "received_at", "move_state", "consumed"], schema: true)
        let values = try decoder.container(keyedBy: CodingKeys.self)
        schema = try values.decode(UInt8.self, forKey: .schema)
        token = try values.decode(String.self, forKey: .token)
        port = try values.decode(UInt16.self, forKey: .port)
        records = try values.decode([Record].self, forKey: .records)
        for record in records { try validateReceivedSymlink(record, records: records) }
        urls = try values.decode([String].self, forKey: .urls)
        received_at = try values.decodeIfPresent(Double.self, forKey: .received_at)
        move_state = try values.decodeIfPresent(String.self, forKey: .move_state)
        guard move_state == nil || move_state == "pending" || move_state == "observed" || move_state == "removed" else { throw CocoaError(.fileReadCorruptFile) }
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
    var version: UInt64? = nil
}
func bridgeRequest(_ operation: String, catalog: Catalog, record: Record, cursor: DirectoryCursor? = nil) throws -> Data {
    guard !isPreparedPlaceholder(record), let node = record.node, let batch = record.id.split(separator: "-").first else { throw NSFileProviderError(.noSuchItem) }
    var bytes = try JSONEncoder().encode(BridgeRequest(operation: operation, token: catalog.token, batch: String(batch), node: node, cursor: cursor, version: operation.hasPrefix("received-") ? record.version : nil))
    bytes.append(10)
    return bytes
}
// EOF/handoff is the receive completion boundary (user decision 2026-10-29).
// The backend records FinalLocal without opening the native copy or waiting for
// File Provider policy acceptance. The received journal still records a handoff
// when the backend has exited; restart discards unfinished one-shot receives.
private let receivedCompletionLog = Logger(subsystem: providerLogSubsystem, category: "received-completion")
func notifyReceived(_ operation: String, record: Record, in root: URL) {
    do {
        let key = receivedKey(record.id)
        let value = try JSONSerialization.jsonObject(with: Data(contentsOf: root.appendingPathComponent(key + ".json"))) as? [String: Any]
        guard let source = value?["source"] as? [String: Any], let fulfilled = source["fulfilled"] as? [[String: Any]], !fulfilled.isEmpty else { return }
        let roots = Set(fulfilled.compactMap { ($0["node"] as? NSNumber)?.uint64Value })
        guard let catalog = try rawCatalogs(in: root).first(where: { $0.records.contains { $0.id == record.id } }) else { return }
        var current = record
        var seen = Set<String>()
        while current.node.map({ !roots.contains($0) }) ?? true {
            guard seen.insert(current.id).inserted, let parent = catalog.records.first(where: { $0.id == current.parent }) else { return }
            current = parent
        }
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { return }
        defer { close(fd) }
        var timeout = timeval(tv_sec: 2, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout.size(ofValue: timeout)))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout.size(ofValue: timeout)))
        var noSigPipe: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout.size(ofValue: noSigPipe)))
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size); address.sin_family = sa_family_t(AF_INET)
        address.sin_port = catalog.port.bigEndian; address.sin_addr.s_addr = inet_addr("127.0.0.1")
        let connected = withUnsafePointer(to: &address) { p in p.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) } }
        guard connected == 0 else { return }
        let request = try bridgeRequest(operation, catalog: catalog, record: record)
        try request.withUnsafeBytes { bytes in
            var offset = 0
            while offset < bytes.count {
                let count = Darwin.write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                guard count > 0 else { throw CocoaError(.fileWriteUnknown) }
                offset += count
            }
        }
        var ack: UInt8 = 255
        let accepted = Darwin.read(fd, &ack, 1) == 1 && ack == 0
        receivedCompletionLog.notice("operation=received-completion accepted=\(accepted)")
    } catch { receivedCompletionLog.notice("operation=received-completion accepted=false") }
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
        case 6: throw CocoaError(.fileReadNoPermission)
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
    return DirectoryListing(records: result.records.map { $0.withCreationDate(catalog.receivedMilliseconds) }, next: result.next, version: result.version)
}
func appGroupContainer() throws -> URL {
    guard let group = Bundle.main.object(forInfoDictionaryKey: providerGroupKey) as? String, group == approvedAppGroup,
          let url = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: group) else {
        throw NSError(domain: "OinkFileProvider", code: 1, userInfo: [NSLocalizedDescriptionKey: "App Group unavailable"])
    }
    return url
}
func container() throws -> URL {
    let upgrade = ProviderUpgrade(group: try appGroupContainer())
    try upgrade.prepareLocal()
    return upgrade.root
}
func catalogs() throws -> [Catalog] { try catalogs(in: container()) }
private func rawCatalogs(in root:URL) throws -> [Catalog] {
    try FileManager.default.contentsOfDirectory(at:root,includingPropertiesForKeys:nil)
        .filter {$0.pathExtension=="json"}.map { url in
            let catalog = try JSONDecoder().decode(Catalog.self, from: Data(contentsOf: url))
            // Source folders have no file payload or delivery received_at. For
            // an existing undated source, its catalog's real creation time is
            // the available persistent date; do not use the enumeration clock.
            let undatedSource = catalog.records.contains {
                $0.directory && $0.node == nil && $0.id.hasPrefix("source") && $0.modified == nil && $0.created == nil
            }
            let born = (undatedSource ? try? url.resourceValues(forKeys: [.creationDateKey]).creationDate : nil)
                .map { Int64($0.timeIntervalSince1970 * 1000) }
            let records = catalog.records.map { record in
                let dated = record.directory && record.node == nil && record.id.hasPrefix("source")
                    ? record.withCreationDate(record.modified ?? born) : record
                // Empty, unbacked preparation catalogs contain no user nodes.
                // Correct their visibility without renaming an existing path;
                // real received empty folders still have a user directory node.
                if catalog.received_at == nil && catalog.records.count == 1,
                   record.directory, record.node == nil, record.children == 0, record.id.hasSuffix("-root") {
                    return Record(id: dated.id, parent: dated.parent, name: dated.name, directory: true,
                                  size: dated.size, modified: dated.modified, node: nil, version: dated.version,
                                  children: dated.children, hidden: true, created: dated.created)
                }
                return dated
            }
            return Catalog(token: catalog.token, port: catalog.port, records: records,
                           urls: catalog.urls, received_at: catalog.received_at, move_state: catalog.move_state)
        }
}
func catalogs(in root: URL) throws -> [Catalog] {
    if let upgrade = providerUpgrade(in: root), try upgrade.pending() { return [] }
    return try withReceived(in: root) { try receivedCatalogs(in: root) }
}
// Caller holds received.lock. Local imports live in the overlay so Rust can
// update source metadata without losing edits or creating a second content store.
private func receivedCatalogs(in root: URL) throws -> [Catalog] {
    let raw = try rawCatalogs(in: root)
    let entries = try receivedEntries(raw.flatMap { $0.records }, in: root)
    let expanded = raw.map { catalog -> Catalog in
        let ids = Set(catalog.records.map(\.id))
        let keys = Set(catalog.records.map { receivedKey($0.id) })
        let added = entries.values.filter {
            $0.modified_locally == true && keys.contains(receivedKey($0.record.id)) && !ids.contains($0.record.id)
        }.map(\.record).sorted { $0.id < $1.id }
        return Catalog(token: catalog.token, port: catalog.port, records: catalog.records + added,
                       urls: catalog.urls, received_at: catalog.received_at, move_state: catalog.move_state)
    }
    let records = expanded.flatMap { $0.records }.map {
        (entries[$0.id]?.record ?? $0).retainingLocal($0.keep_local == true)
    }
    var deleted = Set(entries.filter { $0.value.deleted }.map { $0.key })
    var previous = -1
    while previous != deleted.count {
        previous = deleted.count
        for record in records where deleted.contains(record.parent) { deleted.insert(record.id) }
    }
    let live = records.filter { !deleted.contains($0.id) }
    let byID = Dictionary(live.map { ($0.id, $0) }, uniquingKeysWith: { _, new in new })
    var needed = Set(live.filter { $0.node != nil || $0.hidden == true || !isReceivedContainer($0) }.map(\.id))
    var queue = Array(needed)
    while let id = queue.popLast(), let record = byID[id] {
        if byID[record.parent] != nil && needed.insert(record.parent).inserted { queue.append(record.parent) }
    }
    return expanded.map { catalog in
        Catalog(token: catalog.token, port: catalog.port, records: catalog.records.compactMap { record in
            needed.contains(record.id) ? (entries[record.id]?.record ?? record)
                .retainingLocal(record.keep_local == true).withCreationDate(catalog.receivedMilliseconds ?? record.created) : nil
        }, urls: catalog.urls, received_at: catalog.received_at, move_state: catalog.move_state)
    }
}
final class Item: NSObject, NSFileProviderItem {
    let record: Record
    init(_ record: Record) { self.record = record }
    var itemIdentifier: NSFileProviderItemIdentifier { .init(record.id) }
    var parentItemIdentifier: NSFileProviderItemIdentifier { .init(record.parent) }
    var filename: String { record.name }
    var symlinkTargetPath: String? { record.symlink_target }
    var contentType: UTType { record.symlink_target != nil ? .symbolicLink : record.directory ? .folder : (UTType(filenameExtension: (record.name as NSString).pathExtension) ?? .data) }
    var documentSize: NSNumber? { record.size.map { NSNumber(value: $0) } }
    var childItemCount: NSNumber? { record.children.map { NSNumber(value: $0) } }
    // Source mtime is authoritative when supplied. Otherwise the persisted first
    // publication time describes the received copy; never synthesize Date() on
    // an enumeration, which would change anchors and versions while idle.
    var creationDate: Date? { (record.created ?? record.modified).map { Date(timeIntervalSince1970: Double($0) / 1000) } }
    var contentModificationDate: Date? { (record.modified ?? record.created).map { Date(timeIntervalSince1970: Double($0) / 1000) } }
    // These snapshots export ordinary copies. POSIX userWritable alone still produces uchg
    // on macOS 14 and 26 when capabilities only allow reading; Finder carries it to copies.
    // Declare ordinary operations too, including rename/move, which an immutable item forbids.
    // Reading/writing alias content enumeration/adding children on directories. The extension
    // accepts local mutations only; it never writes changes back to the source.
    var capabilities: NSFileProviderItemCapabilities {
        var value:NSFileProviderItemCapabilities=[.allowsReading,.allowsWriting,.allowsTrashing,.allowsDeleting]
        if record.node != nil || record.id.contains("-local-") {value.formUnion([.allowsRenaming,.allowsReparenting])}
        return value
    }
    var contentPolicy: NSFileProviderContentPolicy {
        record.keep_local == true ? .downloadEagerlyAndKeepDownloaded : .downloadLazily
    }
    var fileSystemFlags: NSFileProviderFileSystemFlags {
        var flags: NSFileProviderFileSystemFlags = record.directory ? [.userReadable, .userWritable, .userExecutable] : [.userReadable, .userWritable]
        if (record.exec_bits ?? 0) & 0o100 != 0 { flags.insert(.userExecutable) }
        if record.hidden == true { flags.insert(.hidden) }
        return flags
    }
    var itemVersion: NSFileProviderItemVersion {
        let size = record.size.map(String.init) ?? "unknown"
        let modified = record.modified.map(String.init) ?? "unknown"
        let content = "\(record.version)-\(size)-\(modified)" + (record.symlink_target.map { "-link:\($0)" } ?? "") + (record.exec_bits.map { "-exec:\($0)" } ?? "")
        var metadata = "\(content)-\(record.children.map(String.init) ?? "unknown")-\(record.parent)-\(record.name)"
        if let created = record.created { metadata += "-created:\(created)" }
        // A reused provider identity must signal the rename and clear UF_HIDDEN
        // even when the real directory still has unknown size and version 1.
        // Generic -> final is metadata; never convert a file identity to folder.
        if record.id.contains("-slot-") || record.hidden != nil {
            metadata += "-\(record.name)-\(record.hidden == true)"
        }
        if record.keep_local == true { metadata += "-keep-local" }
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
        let records = try catalogs(in:root).flatMap { $0.records }.sorted { $0.id < $1.id }
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
            defer { cacheStorageChanged() }
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
            cacheStorageChanged()
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
        guard lstat(url.path, &metadata) == 0 else { result.incomplete = true; continue }
        if metadata.st_mode & S_IFMT == S_IFLNK && url.lastPathComponent.hasPrefix("handoff-") {
            // Symlink handoff has no payload bytes and must never be opened.
            if clear && evicted.contains(where: { url.lastPathComponent.hasPrefix("handoff-" + $0 + "-") }) {
                guard unlink(url.path) == 0 else { result.incomplete = true; continue }
                cacheStorageChanged()
            }
            continue
        }
        guard metadata.st_mode & S_IFMT == S_IFREG else { result.incomplete = true; continue }
        let size = UInt64(max(0, metadata.st_blocks)) * 512
        let fd = Darwin.open(url.path, O_RDONLY | O_NOFOLLOW)
        defer { if fd >= 0 { close(fd) } }
        // A returned URL stays under the system's control until the corresponding
        // materialized item has been successfully evicted. Busy writers are skipped.
        let handoff = url.lastPathComponent.hasPrefix("handoff-")
        let busy = (handoff && !evicted.contains(where: { url.lastPathComponent.hasPrefix("handoff-" + $0 + "-") })) || fd < 0 || flock(fd, LOCK_EX | LOCK_NB) != 0
        if clear && !busy {
            precondition(url.deletingLastPathComponent().standardizedFileURL == root.standardizedFileURL)
            do { try FileManager.default.removeItem(at: url); cacheStorageChanged(); result.removed += size; continue } catch {}
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
        cacheStorageChanged()
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
    guard try materialized(url) else { throw ItemRejection.metadataMismatch }
    return url
}

// Fixed cause codes cross only the one-shot manager's exit status, never paths.
enum ItemRejection: Int32, Error {
    case missing = 32, unsupportedType, wrongKind, wrongTarget, identityMismatch, metadataMismatch, accessDenied, metadataUnreadable
}
enum ItemReadiness { case ready, pending, rejected(ItemRejection) }
func itemReadiness(_ record: Record, url: URL, metadata: VisibleMetadata?) -> ItemReadiness {
    // A cached private/generic URL or a hidden prepared slot can legitimately
    // precede its final rename/flag update. Those are the only identity stages
    // for which polling is useful; a final visible item must have the right kind.
    guard url.lastPathComponent == record.name else { return .pending }
    guard let metadata else { return .rejected(.missing) }
    guard metadata.hidden == (record.hidden == true) else { return .pending }
    guard metadata.directory == record.directory else { return .rejected(.wrongKind) }
    guard metadata.symlinkTarget == record.symlink_target else {
        return .rejected(metadata.symlinkTarget == nil || record.symlink_target == nil ? .wrongKind : .wrongTarget)
    }
    // macOS 14/26 can expose the final name before refreshing documentSize.
    guard record.directory || record.size == nil || record.size == metadata.size else { return .pending }
    return .ready
}
func publicationRejection(_ error: Error) -> ItemRejection? {
    if let rejection = error as? ItemRejection { return rejection }
    let error = error as NSError
    if (error.domain == NSCocoaErrorDomain && [CocoaError.fileNoSuchFile.rawValue, CocoaError.fileReadNoSuchFile.rawValue].contains(error.code)) ||
        (error.domain == NSFileProviderErrorDomain && error.code == NSFileProviderError.noSuchItem.rawValue) { return .missing }
    if error.domain == NSCocoaErrorDomain && error.code == CocoaError.fileReadNoPermission.rawValue { return .accessDenied }
    if error.domain == NSCocoaErrorDomain && error.code == CocoaError.fileReadCorruptFile.rawValue { return .metadataMismatch }
    return nil
}

struct VisibleMetadata {
    let directory: Bool
    let size: UInt64
    let hidden: Bool
    let symlinkTarget: String?
    init(directory: Bool, size: UInt64, hidden: Bool, symlinkTarget: String? = nil) {
        self.directory = directory; self.size = size; self.hidden = hidden; self.symlinkTarget = symlinkTarget
    }
}
func finalItemMatches(_ record: Record, url: URL, metadata: VisibleMetadata?) -> Bool {
    guard let metadata, url.lastPathComponent == record.name,
          metadata.directory == record.directory, metadata.hidden == (record.hidden == true),
          metadata.symlinkTarget == record.symlink_target else { return false }
    return record.directory || record.size == nil || record.size == metadata.size
}

func isPreparedPlaceholder(_ record: Record) -> Bool { record.hidden == true && record.node != nil }
func inspectVisibleMetadata(_ url: URL) throws -> VisibleMetadata? {
    let scoped = url.startAccessingSecurityScopedResource()
    defer { if scoped { url.stopAccessingSecurityScopedResource() } }
    var metadata = stat()
    guard lstat(url.path, &metadata) == 0 else {
        if errno == ENOENT { return nil }
        throw errno == EACCES || errno == EPERM ? ItemRejection.accessDenied : ItemRejection.metadataUnreadable
    }
    let kind = metadata.st_mode & mode_t(S_IFMT)
    // A root link is ready by its own lstat/readlink metadata, even before the
    // other selected root is materialized. Rejecting S_IFLNK here kept an entire
    // mixed batch waiting until bound_expired. Do not follow or hydrate targets.
    let target: String?
    if kind == mode_t(S_IFLNK) {
        do { target = try FileManager.default.destinationOfSymbolicLink(atPath: url.path) }
        catch { throw publicationRejection(error) ?? .metadataUnreadable }
    } else {
        guard kind == mode_t(S_IFREG) || kind == mode_t(S_IFDIR) else { throw ItemRejection.unsupportedType }
        target = nil
    }
    return VisibleMetadata(directory: kind == mode_t(S_IFDIR), size: UInt64(max(0, metadata.st_size)),
                           hidden: metadata.st_flags & UInt32(UF_HIDDEN) != 0, symlinkTarget: target)
}
// Optional metadata is convenient for non-waiting inventory probes. Readiness
// and retirement must use the throwing inspector, never swallow its rejection.
func visibleMetadata(_ url: URL) -> VisibleMetadata? { try? inspectVisibleMetadata(url) }
func waitForItemRemoval(_ url: URL, inspect: (URL) throws -> VisibleMetadata? = inspectVisibleMetadata,
                        pause: () async throws -> Void = { try await Task.sleep(nanoseconds: 10_000_000) }) async throws {
    while try inspect(url) != nil { try Task.checkCancellation(); try await pause() }
}
func resolveFinalItemURL(_ record: Record, parent: URL? = nil, resolve: () async throws -> URL,
                         inspect: (URL) throws -> VisibleMetadata? = inspectVisibleMetadata,
                         pause: () async throws -> Void = { try await Task.sleep(nanoseconds: 10_000_000) }) async throws -> URL {
    // macOS 14/26 return a cached generic URL immediately after a WS rename.
    // Waiting only for the first callback hands native readers the wrong name,
    // zero size and sometimes an inode that the daemon is about to replace.
    // The manager watchdog and owning Rust publication bound/cancel this loop.
    while true {
        try Task.checkCancellation()
        let url: URL
        do { url = try await resolve() }
        catch { if let rejection = publicationRejection(error) { throw rejection }; throw error }
        if parent == nil || url.deletingLastPathComponent().standardizedFileURL == parent!.standardizedFileURL {
            switch itemReadiness(record, url: url, metadata: try inspect(url)) {
            case .ready: return url
            case .pending: break
            case .rejected(let reason): throw reason
            }
        }
        try await pause()
    }
}

func resolvePublishedItemURLs(parent: Record, children: [Record],
                              resolve: @escaping (Record) async throws -> URL,
                              inspect: @escaping (URL) throws -> VisibleMetadata? = inspectVisibleMetadata,
                              rejected: @escaping (ItemRejection) -> Void = { _ in },
                              pause: @escaping () async throws -> Void = { try await Task.sleep(nanoseconds: 10_000_000) }) async throws -> [String] {
    // A child's final basename/size can be visible while its parent still has
    // the private spare name. Resolving both concurrently can return child URLs
    // invalidated by that parent's rename before native drag begins. Resolve the
    // parent first, then require each child to belong to that resolved directory.
    let directory: URL
    do { directory = try await resolveFinalItemURL(parent, resolve: { try await resolve(parent) }, inspect: inspect, pause: pause) }
    catch { if let rejection = publicationRejection(error) { rejected(rejection) }; throw error }
    return try await resolveItemURLs(children) { child in
        do { return try await resolveFinalItemURL(child, parent: directory,
                                      resolve: { try await resolve(child) }, inspect: inspect, pause: pause).path }
        catch { if let rejection = publicationRejection(error) { rejected(rejection) }; throw error }
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

protocol ProviderItemDomain {
    func signalEnumerator(for containerItemIdentifier: NSFileProviderItemIdentifier) async throws
    func getUserVisibleURL(for itemIdentifier: NSFileProviderItemIdentifier) async throws -> URL
}
extension NSFileProviderManager: ProviderItemDomain {}

func retirePlaceholder(batch: String, ledger: PlaceholderLedger, manager: ProviderItemDomain,
                       inspect: (URL) throws -> VisibleMetadata? = inspectVisibleMetadata,
                       pause: () async throws -> Void = { try await Task.sleep(nanoseconds: 50_000_000) }) async throws {
    try await manager.signalEnumerator(for: .workingSet)
    // waitForStabilization is a testing-only, whole-domain barrier. On macOS
    // 14.7.8/26.6.2, unrelated reconciliation kept it pending even after these
    // paths were gone; helper exit did not remove the daemon's waiter. Retrying
    // every 12s accumulated thousands. Use this one-shot identity's deletion,
    // not domain quiescence; keep the deadline/ledger on every real failure.
    let parent: URL
    if let known = ledger.parent { parent = URL(fileURLWithPath: known) }
    else {
        let root = try await manager.getUserVisibleURL(for: .rootContainer)
        parent = root.appendingPathComponent(".Oink-prepared-" + batch + ".noindex")
    }
    var paths = Set([parent])
    let item = NSFileProviderItemIdentifier(batch + "-root")
    // A signal ACK only schedules enumeration. ENOENT alone can precede an
    // asynchronous creation, or refer to the old name during activation. The
    // public URL lookup goes through fileproviderd and requests materialization
    // if the identity still exists (macOS 26 framework inspected). Drain that
    // identity first, retaining any renamed URL, then check namespace removal.
    // Do not accept transport/auth errors, generic missing-file errors, or a
    // successful URL callback as deletion. No content read/download is needed.
    while true {
        try Task.checkCancellation()
        do { paths.insert(try await manager.getUserVisibleURL(for: item)) }
        catch {
            let native = error as NSError
            guard native.domain == NSFileProviderErrorDomain,
                  native.code == NSFileProviderError.noSuchItem.rawValue else { throw error }
            break
        }
        try await pause()
    }
    for path in paths { try await waitForItemRemoval(path, inspect: inspect, pause: pause) }
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
                           reconnect: () async throws -> Void,
                           finishUpgrade: () async throws -> Void,
                           resolveRoot: () async throws -> URL) async throws {
    guard hidden else { throw CocoaError(.fileReadUnknown) }
    guard enabled else { return }
    if !disconnected { try await disconnect([.temporary]) }
    try await reconnect()
    try await finishUpgrade()
    // Verify namespace access, not completion of all retained transfers. The
    // host restores its content bridge only after this callback returns; those
    // transfers may still reference the previous process's localhost port.
    // waitForStabilization here therefore makes startup wait on itself.
    try await verifyProviderLaunch(hidden: hidden, enabled: enabled, disconnected: false,
                                   resolveRoot: resolveRoot)
}

func verifyProviderLaunch(hidden: Bool, enabled: Bool, disconnected: Bool,
                          resolveRoot: () async throws -> URL) async throws {
    guard hidden, enabled, !disconnected else { throw NSFileProviderError(.serverUnreachable) }
    // Root URL resolution exercises the native namespace without reading file
    // contents, materializing an item, or waiting on unrelated remote work.
    let root = try await resolveRoot()
    guard root.isFileURL else { throw CocoaError(.fileReadUnknown) }
}

enum ProviderRemovalError: Error {
    case preservedData
    case domainsRemain
}

enum ProviderRemovalAction: String {
    case remove = "remove-domain"

    func domains(_ registered: [NSFileProviderDomain], uninstall: Bool) -> [NSFileProviderDomain] {
        // One active domain. Explicit uninstall also tolerates an installed
        // build 48's abandoned domain; it is never used for new publications.
        registered.filter {
            $0.identifier.rawValue == providerDomain || (uninstall && $0.identifier.rawValue == providerDomainBase + "-links")
        }
    }

    func perform(domains: [NSFileProviderDomain],
                 remove: (NSFileProviderDomain, NSFileProviderManager.DomainRemovalMode) async throws -> URL?,
                 remaining: () async throws -> [NSFileProviderDomain]) async throws -> String {
        guard !domains.isEmpty else { return "absent" }
        var failure: Error?
        // The list is scoped to this extension, not every provider on the Mac.
        // Remove each selected domain regardless of visibility or connection.
        for domain in domains {
            do {
                // Explicit uninstall discards managed received files, including
                // local edits. Preservation modes leave those files behind and
                // cannot acknowledge complete product-data removal.
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

// Cross-process mutation overlay: Rust owns the source catalog; the extension
// owns received-file mutations. A stale bridge write cannot resurrect a deletion.
struct ReceivedEntry: Codable {
    var record: Record
    var received_at: Double
    var handed_off: Bool
    var materialized: Bool
    var deleted: Bool
    var fetching:Int? = nil
    var modified_locally:Bool? = nil
    var original_parent:String? = nil
    var imported_as:String? = nil
    var imported_version:Data? = nil
}
// `hidden` is reserved for our private prepared parents/slots, not user file
// visibility. Keep their OS materialization outside the received-file overlay.
func receivedRecordEligible(_ record: Record) -> Bool { record.hidden != true }
func isReceivedContainer(_ record: Record) -> Bool {
    record.directory && record.node == nil && (record.id.hasPrefix("source") || record.id.hasSuffix("-root"))
}
func receivedKey(_ id: String) -> String { String(id.split(separator:"-").first ?? "") }
private func receivedFile(_ key:String, in root:URL) -> URL { root.appendingPathComponent(key).appendingPathExtension("received") }
func withReceived<T>(in root:URL, _ operation:() throws -> T) throws -> T {
    if let upgrade = providerUpgrade(in: root) {
        return try upgrade.withLocalLock {
            try upgrade.requireReady()
            return try withReceivedLock(in: root, operation)
        }
    }
    return try withReceivedLock(in: root, operation)
}
private func withReceivedLock<T>(in root:URL, _ operation:() throws -> T) throws -> T {
    let fd=Darwin.open(root.appendingPathComponent("received.lock").path,O_CREAT|O_RDWR,0o600)
    guard fd >= 0 else { throw CocoaError(.fileWriteUnknown) }
    defer { close(fd) }
    guard flock(fd,LOCK_EX)==0 else { throw CocoaError(.fileWriteUnknown) }
    defer { flock(fd,LOCK_UN) }
    return try operation()
}
func readReceived(_ key:String,in root:URL) throws -> [String:ReceivedEntry] {
    let file=receivedFile(key,in:root)
    guard FileManager.default.fileExists(atPath:file.path) else { return [:] }
    // Prepared parents are intentionally materialized to warm the namespace.
    // That is not receipt of user data. Their private snapshots must never
    // override the final rename/reparent or protect a cancelled publication.
    return try JSONDecoder().decode([String:ReceivedEntry].self,from:Data(contentsOf:file))
        .filter { receivedRecordEligible($0.value.record) }
}
private func writeReceived(_ key:String,_ entries:[String:ReceivedEntry],in root:URL) throws {
    try JSONEncoder().encode(entries).write(to:receivedFile(key,in:root),options:.atomic)
    cacheStorageChanged()
}
func receivedProtected(_ key:String,in root:URL) throws -> Bool {
    let catalogURL=root.appendingPathComponent(key).appendingPathExtension("json")
    if let catalog=try? JSONDecoder().decode(Catalog.self,from:Data(contentsOf:catalogURL)),catalog.received_at==nil {return false}
    return try withReceived(in:root) { try readReceived(key,in:root).values.contains { !$0.deleted && ($0.record.node != nil || $0.modified_locally == true) && ($0.handed_off || $0.materialized || ($0.fetching ?? 0)>0) } }
}
// Caller holds received.lock. Read each overlay once, including source-folder
// tombstones; a late directory page cannot revive a deleted ancestor.
private func receivedEntries(_ records:[Record],in root:URL) throws -> [String:ReceivedEntry] {
    var result:[String:ReceivedEntry]=[:]
    for key in Set(records.map {receivedKey($0.id)}) {result.merge(try readReceived(key,in:root),uniquingKeysWith: {_,new in new})}
    return result
}
private func receivedDeleted(_ record:Record,in root:URL) throws -> Bool {
    let raw=try rawCatalogs(in:root).flatMap {$0.records}
    let entries=try receivedEntries(raw+[record],in:root)
    let records=Dictionary(raw.map {($0.id,$0)},uniquingKeysWith: {_,new in new})
    var id=record.id;var visited=Set<String>()
    while visited.insert(id).inserted {
        if entries[id]?.deleted==true {return true}
        guard let current=entries[id]?.record ?? records[id] ?? (id==record.id ? record : nil) else {break}
        id=current.parent
    }
    return false
}
func receivedOverlay(_ record:Record,in root:URL) throws -> Record? {
    try withReceived(in:root) {
        if try receivedDeleted(record,in:root) {return nil}
        return (try readReceived(receivedKey(record.id),in:root)[record.id]?.record ?? record).retainingLocal(record.keep_local == true)
    }
}
private func receivedTime(_ id:String,in root:URL,fallback:Date=Date()) -> Double {
    let path=root.appendingPathComponent(receivedKey(id)).appendingPathExtension("json")
    return (try? JSONDecoder().decode(Catalog.self,from:Data(contentsOf:path)).received_at) ?? fallback.timeIntervalSince1970*1000
}
func receivedDirectoryHandoff(_ requested: Record, in root: URL) throws {
    // A directory listing can bind a newer view/version. Report that received
    // manifest rather than the stale record that initiated enumeration.
    guard let current = try rawCatalogs(in: root).flatMap({ $0.records }).first(where: { $0.id == requested.id }) else { throw NSFileProviderError(.noSuchItem) }
    try receivedHandoff(current, in: root)
}
func receivedHandoff(_ record:Record,in root:URL,at date:Date=Date()) throws {
    try withReceived(in:root) {
        let key=receivedKey(record.id)
        var entries=try readReceived(key,in:root)
        if try receivedDeleted(record,in:root) { throw NSFileProviderError(.noSuchItem) }
        var entry=entries[record.id] ?? ReceivedEntry(record:record,received_at:receivedTime(record.id,in:root,fallback:date),handed_off:false,materialized:false,deleted:false)
        // A fetch may have started before modifyItem committed local contents.
        // Decide under the same cross-process lock as the edit; handing the old
        // source URL to File Provider would replace the user's final local copy.
        guard entry.modified_locally != true else { throw NSFileProviderError(.versionNoLongerAvailable) }
        entry.record=record;entry.handed_off=true;entry.fetching=max(0,(entry.fetching ?? 0)-1);entries[record.id]=entry
        try writeReceived(key,entries,in:root)
    }
    notifyReceived("received-complete", record: record, in: root)
}
func receivedDelete(_ ids:[String],in root:URL) throws {
    try withReceived(in:root) {
        let raw=try FileManager.default.contentsOfDirectory(at:root,includingPropertiesForKeys:nil)
            .filter {$0.pathExtension=="json"}.flatMap { try JSONDecoder().decode(Catalog.self,from:Data(contentsOf:$0)).records }
        for (key,group) in Dictionary(grouping:ids,by:receivedKey) {
            var entries=try readReceived(key,in:root)
            for id in group {
                if var entry=entries[id] { entry.deleted=true;entries[id]=entry }
                else if let record=raw.first(where:{$0.id==id}) { entries[id]=ReceivedEntry(record:record,received_at:receivedTime(id,in:root),handed_off:false,materialized:false,deleted:true) }
            }
            try writeReceived(key,entries,in:root)
        }
    }
}
private func receivedName(_ name: String) throws {
    guard !name.isEmpty, name != ".", name != "..", !name.contains("/"), !name.contains("\0") else {
        throw CocoaError(.fileWriteInvalidFileName)
    }
}
private func receivedContentSize(_ contents: URL) throws -> UInt64 {
    var metadata = stat()
    guard lstat(contents.path, &metadata) == 0, metadata.st_mode & S_IFMT == S_IFREG,
          metadata.st_flags & UInt32(SF_DATALESS) == 0, metadata.st_size >= 0 else {
        throw CocoaError(.fileReadUnknown)
    }
    return UInt64(metadata.st_size)
}
func receivedCreate(_ template: NSFileProviderItem, fields: NSFileProviderItemFields, contents: URL?,
                    options: NSFileProviderCreateItemOptions, in root: URL) throws -> Record {
    try withReceived(in: root) {
        guard fields.contains(.filename), fields.contains(.parentItemIdentifier) else {
            throw CocoaError(.fileWriteInvalidFileName)
        }
        let name = template.filename
        try receivedName(name)
        let parentID = template.parentItemIdentifier.rawValue
        let raw = try rawCatalogs(in: root).flatMap { $0.records }
        let entries = try receivedEntries(raw, in: root)
        let records = try receivedCatalogs(in: root).flatMap { $0.records }
        guard let parent = records.first(where: { $0.id == parentID && $0.directory }) ?? raw.first(where: { $0.id == parentID && $0.directory }),
              try !receivedDeleted(parent, in: root),
              parent.hidden != true, !parent.id.hasPrefix("source"),
              !parent.id.contains("/"), !parent.id.contains("\0") else { throw CocoaError(.fileWriteNoPermission) }
        let key = receivedKey(parent.id)
        let imported = template.itemIdentifier.rawValue
        let importedVersion = (template.itemVersion ?? nil)?.contentVersion
        // The OS creation identifier is stable across replay, but is not a
        // provider identifier. Only deletion-conflict explicitly names an old ID.
        if let replay = entries.values.first(where: { $0.imported_as == imported && $0.imported_version == importedVersion && receivedKey($0.record.id) == key && !$0.deleted }) {
            return replay.record
        }
        let collision = records.first { $0.parent == parent.id && $0.name.caseInsensitiveCompare(name) == .orderedSame }
        let old: Record?
        if options.contains(.deletionConflicted) {
            guard let existing = entries[imported]?.record ?? raw.first(where: { $0.id == imported }),
                  receivedKey(existing.id) == key, !isReceivedContainer(existing) else { throw CocoaError(.fileWriteNoPermission) }
            old = existing
        } else { old = nil }
        if let collision = collision, collision.id != old?.id {
            if options.contains(.mayAlreadyExist) && contents == nil,
               collision.directory == ((template.contentType ?? nil)?.conforms(to: .folder) == true) { return collision }
            throw NSError.fileProviderErrorForCollision(with: Item(collision))
        }
        let directory = (template.contentType ?? nil)?.conforms(to: .folder) == true
        guard (template.contentType ?? nil)?.conforms(to: .symbolicLink) != true else { throw CocoaError(.fileWriteNoPermission) }
        guard directory || contents != nil else { throw NSFileProviderError(.excludedFromSync) }
        let size = try contents.map(receivedContentSize)
        if let old = old, old.directory != directory { throw CocoaError(.fileWriteNoPermission) }
        let id = old?.id ?? key + "-local-" + UUID().uuidString.replacingOccurrences(of: "-", with: "").lowercased()
        let now = Int64(Date().timeIntervalSince1970 * 1000)
        let record = Record(id: id, parent: parent.id, name: name, directory: directory, size: directory ? nil : size,
                            modified: now, node: old?.node, version: (old?.version ?? 0) &+ 1,
                            children: directory ? 0 : nil, created: old?.created ?? now, keep_local: true)
        var values = try readReceived(key, in: root)
        var entry = ReceivedEntry(record: record, received_at: receivedTime(id, in: root),
                                  handed_off: true, materialized: true, deleted: false)
        entry.modified_locally = true
        entry.imported_as = imported
        entry.imported_version = importedVersion
        // Keep in-flight counters until their stale fetches unwind normally.
        entry.fetching = values[id]?.fetching
        values[id] = entry
        try writeReceived(key, values, in: root)
        return record
    }
}
func receivedLocallyModified(_ record: Record, in root: URL) throws -> Bool {
    try readReceived(receivedKey(record.id), in: root)[record.id]?.modified_locally == true
}
func receivedSourceFailure(_ error: Error, for id: String, in root: URL) throws -> Error {
    let native = error as NSError
    guard native.domain == NSFileProviderErrorDomain, native.code == NSFileProviderError.noSuchItem.rawValue else { return error }
    return try withReceived(in: root) {
        let records = try receivedCatalogs(in: root).flatMap { $0.records }
        let entries = try receivedEntries(records, in: root)
        let byID = Dictionary(records.map { ($0.id, $0) }, uniquingKeysWith: { _, new in new })
        for record in records where entries[record.id]?.modified_locally == true {
            var cursor = record.id
            var seen = Set<String>()
            while seen.insert(cursor).inserted {
                if cursor == id { return NSFileProviderError(.versionNoLongerAvailable) }
                guard let parent = byID[cursor]?.parent else { break }
                cursor = parent
            }
        }
        return error
    }
}
func receivedDirectoryListing(_ listing: DirectoryListing, directory: Record, in root: URL) throws -> DirectoryListing {
    try withReceived(in: root) {
        if try receivedDeleted(directory, in: root) { throw NSFileProviderError(.noSuchItem) }
        if try receivedLocallyModified(directory, in: root) { throw NSFileProviderError(.versionNoLongerAvailable) }
        let raw = try rawCatalogs(in: root).flatMap { $0.records }
        for record in listing.records { try validateReceivedSymlink(record, records: raw + listing.records) }
        let entries = try receivedEntries(raw + listing.records, in: root)
        var records: [Record] = []
        for source in listing.records {
            let record = (entries[source.id]?.record ?? source).retainingLocal(source.keep_local == true)
            if record.parent == directory.id, try !receivedDeleted(record, in: root) { records.append(record) }
        }
        if listing.next == nil {
            // Add imports and nodes reparented here once, on the final page.
            let ids = Set(records.map(\.id))
            records += entries.values.filter { entry in
                entry.record.parent == directory.id && !entry.deleted && !ids.contains(entry.record.id)
                    && !raw.contains { $0.id == entry.record.id && $0.parent == directory.id }
            }.map(\.record).sorted { $0.id < $1.id }
        }
        return DirectoryListing(records: records, next: listing.next, version: listing.version)
    }
}
func receivedModify(_ item:NSFileProviderItem,fields:NSFileProviderItemFields,contents:URL?,in root:URL) throws -> Record {
    try withReceived(in: root) {
        let id = item.itemIdentifier.rawValue
        let records = try receivedCatalogs(in: root).flatMap { $0.records }
        guard let old = records.first(where: { $0.id == id }) else { throw NSFileProviderError(.noSuchItem) }
        let parent = fields.contains(.parentItemIdentifier) ? item.parentItemIdentifier.rawValue : old.parent
        let trash = NSFileProviderItemIdentifier.trashContainer.rawValue
        let key = receivedKey(id)
        var entries = try readReceived(key, in: root)
        let saved = entries[id]
        let restoring = old.parent == trash && saved?.original_parent == parent
        let structural = fields.contains(.filename) || fields.contains(.parentItemIdentifier)
        guard !structural || !isReceivedContainer(old) || parent == trash || restoring else { throw CocoaError(.fileWriteNoPermission) }
        guard parent == trash || restoring || receivedKey(parent) == key, parent != id else { throw CocoaError(.fileWriteNoPermission) }
        guard parent == trash || restoring || records.contains(where: { $0.id == parent && $0.directory }) else { throw CocoaError(.fileWriteNoPermission) }
        var cursor = parent
        var seen = Set<String>()
        while let ancestor = records.first(where: { $0.id == cursor }) {
            guard ancestor.id != id, seen.insert(cursor).inserted else { throw CocoaError(.fileWriteInvalidFileName) }
            cursor = ancestor.parent
        }
        let name = fields.contains(.filename) ? item.filename : old.name
        try receivedName(name)
        if let collision = records.first(where: { $0.id != id && $0.parent == parent && $0.name.caseInsensitiveCompare(name) == .orderedSame }) {
            throw NSError.fileProviderErrorForCollision(with: Item(collision))
        }
        let changedContents = fields.contains(.contents)
        guard !changedContents || (!old.directory && old.symlink_target == nil && contents != nil) else { throw CocoaError(.fileReadUnknown) }
        let size = try changedContents ? receivedContentSize(contents!) : old.size
        let record = Record(id:id,parent:parent,name:name,directory:old.directory,size:size,
                            modified:changedContents ? Int64(Date().timeIntervalSince1970 * 1000) : old.modified,
                            node:old.node,version:old.version &+ (changedContents ? 1:0),children:old.children,
                            hidden:old.hidden,created:old.created,keep_local:changedContents ? true : old.keep_local, symlink_target:old.symlink_target, exec_bits:old.exec_bits)
        let updatedRecords = records.map { $0.id == record.id ? record : $0 }
        for candidate in updatedRecords where candidate.symlink_target != nil {
            try validateReceivedSymlink(candidate, records: updatedRecords)
        }
        var entry = saved ?? ReceivedEntry(record:record,received_at:receivedTime(id,in:root),handed_off:false,materialized:false,deleted:false)
        if parent == trash && old.parent != trash { entry.original_parent = old.parent }
        entry.record = record
        if changedContents { entry.handed_off = true; entry.materialized = true; entry.modified_locally = true }
        entries[id] = entry
        try writeReceived(key,entries,in:root)
        return record
    }
}

// Shared with backend leases; the lock inode remains stable across cleanups.
final class ReceivedConsumerExclusion {
    private let fd: Int32
    init?(key: String, root: URL) throws {
        let path = root.appendingPathComponent(key).appendingPathExtension("consumers")
        let opened = Darwin.open(path.path, O_CREAT | O_RDWR | O_NOFOLLOW | O_CLOEXEC, 0o600)
        guard opened >= 0 else { throw POSIXError(.init(rawValue: errno) ?? .EIO) }
        guard flock(opened, LOCK_EX | LOCK_NB) == 0 else {
            let code = errno; close(opened)
            if code == EWOULDBLOCK { return nil }
            throw POSIXError(.init(rawValue: code) ?? .EIO)
        }
        fd = opened
    }
    deinit { flock(fd, LOCK_UN); close(fd) }
}
// The caller has established its age policy. Serialize deletion with every
// backend promise/stream lease, including consumers which have not read yet.
func removeReceivedNative(_ url: URL, record: Record, identity: stat, in root: URL) throws -> Bool {
    guard let exclusion = try ReceivedConsumerExclusion(key: receivedKey(record.id), root: root) else { return false }
    defer { withExtendedLifetime(exclusion) {} }
    var coordinationError: NSError?
    var deletionError: Error?
    NSFileCoordinator().coordinate(writingItemAt: url, options: .forDeleting, error: &coordinationError) { coordinated in
        do {
            // The visible path was resolved before admission/coordination. It
            // may now name a replacement; neither a stale path nor old metadata
            // authorizes deleting that new file. The age policy still applies
            // to local edits, but a concurrent edit defers this cleanup pass.
            let current = try readReceived(receivedKey(record.id), in: root)[record.id]
            var actual = stat()
            guard current?.deleted != true, (current?.fetching ?? 0) == 0,
                  coordinated.standardizedFileURL == url.standardizedFileURL,
                  lstat(coordinated.path, &actual) == 0,
                  actual.st_dev == identity.st_dev, actual.st_ino == identity.st_ino,
                  actual.st_mode & S_IFMT == mode_t(record.directory ? S_IFDIR : record.symlink_target != nil ? S_IFLNK : S_IFREG),
                  actual.st_flags & UInt32(SF_DATALESS) == 0,
                  record.directory || (actual.st_size == identity.st_size &&
                    actual.st_mtimespec.tv_sec == identity.st_mtimespec.tv_sec &&
                    actual.st_mtimespec.tv_nsec == identity.st_mtimespec.tv_nsec &&
                    actual.st_ctimespec.tv_sec == identity.st_ctimespec.tv_sec &&
                    actual.st_ctimespec.tv_nsec == identity.st_ctimespec.tv_nsec) else {
                throw CocoaError(.fileWriteUnknown)
            }
            if record.directory {
                guard rmdir(coordinated.path) == 0 else { throw CocoaError(.fileWriteUnknown) }
            } else {
                guard unlink(coordinated.path) == 0 else { throw CocoaError(.fileWriteUnknown) }
            }
        } catch { deletionError = error }
    }
    if let error = coordinationError { throw error }
    if let error = deletionError { throw error }
    defer { cacheStorageChanged() } // Native unlink committed even if the overlay write fails.
    try receivedDelete([record.id], in: root)
    return true
}

// Enumerating/statting metadata must never read a dataless file's contents.
func receivedUsage(manager:NSFileProviderManager,root:URL,days:UInt64,clear:Bool) async throws -> [String:Any] {
    let observer=MaterializedObserver(manager.enumeratorForMaterializedItems())
    defer {observer.enumerator.invalidate()}
    let items=try await observer.read()
    var bytes:UInt64=0;var incomplete=false;var removed=0
    let records=try catalogs(in:root).flatMap {$0.records}
    for item in items {
        let id=item.itemIdentifier.rawValue
        guard let record=records.first(where:{$0.id==id}), receivedRecordEligible(record),!isReceivedContainer(record) else {continue}
        let entry=try readReceived(receivedKey(id),in:root)[id] ?? ReceivedEntry(record:record,received_at:receivedTime(id,in:root),handed_off:false,materialized:false,deleted:false)
        guard !entry.deleted else {continue}
        do {
            let url=try await manager.getUserVisibleURL(for:item.itemIdentifier)
            var meta=stat();guard lstat(url.path,&meta)==0 else {throw CocoaError(.fileReadUnknown)}
            guard meta.st_flags & UInt32(SF_DATALESS)==0 else {continue}
            if !entry.record.directory {guard meta.st_mode & S_IFMT == (entry.record.symlink_target != nil ? S_IFLNK : S_IFREG),UInt64(max(0,meta.st_size))==entry.record.size else {incomplete=true;continue}}
            try withReceived(in:root) {
                let key=receivedKey(id);var values=try readReceived(key,in:root)
                var current=values[id] ?? entry
                if !current.deleted && !current.materialized {current.materialized=true;values[id]=current;try writeReceived(key,values,in:root)}
            }
            let size=entry.record.directory || entry.record.symlink_target != nil ? 0 : UInt64(max(0,meta.st_blocks))*512
            let due=days>0 && Date().timeIntervalSince1970*1000-entry.received_at >= Double(days)*86_400_000
            if clear && due && (entry.fetching ?? 0)==0 {
                if try removeReceivedNative(url, record: entry.record, identity: meta, in: root) { removed += 1 }
                else { bytes += size }
            } else {bytes+=size}
        } catch {incomplete=true}
    }
    if removed>0 {try await manager.signalEnumerator(for:.workingSet)}
    anchorLog.notice("operation=received-maintain materialized_bytes=\(bytes) removed=\(removed) incomplete=\(incomplete)")
    return ["materialized_bytes":bytes,"incomplete":incomplete,"time_basis":"received"]
}

func receivedBegin(_ record:Record,in root:URL) throws {
    try withReceived(in:root) {
        let key=receivedKey(record.id);var entries=try readReceived(key,in:root)
        if try receivedDeleted(record,in:root) {throw NSFileProviderError(.noSuchItem)}
        var entry=entries[record.id] ?? ReceivedEntry(record:record,received_at:receivedTime(record.id,in:root),handed_off:false,materialized:false,deleted:false)
        guard entry.modified_locally != true else {throw NSFileProviderError(.versionNoLongerAvailable)}
        entry.fetching=(entry.fetching ?? 0)+1;entries[record.id]=entry;try writeReceived(key,entries,in:root)
    }
}
func receivedFetchFailed(_ id:String,in root:URL) throws {
    try withReceived(in:root) {
        let key=receivedKey(id);var entries=try readReceived(key,in:root)
        if var entry=entries[id] {
            entry.fetching=max(0,(entry.fetching ?? 0)-1);entries[id]=entry
            try writeReceived(key,entries,in:root)
            // noSuchItem asks File Provider to remove its local item. A source
            // failure from an older fetch has no authority over a local edit.
            guard entry.modified_locally != true else {throw NSFileProviderError(.versionNoLongerAvailable)}
        }
    }
}

// Same lexical root bound as the sender. Never resolves a target through the FS.
func safeSymlinkTarget(_ target: String, parentDepth: Int) -> Bool {
    guard !target.isEmpty, target.utf8.count <= 4096, !target.hasPrefix("/"), !target.contains("\0") else { return false }
    var depth = parentDepth
    for part in target.split(separator: "/", omittingEmptySubsequences: false) {
        if part == ".." { guard depth > 0 else { return false }; depth -= 1 }
        else if part != "." && !part.isEmpty { depth += 1 }
    }
    return depth > 0
}

func validateReceivedSymlink(_ record: Record, records: [Record]) throws {
    guard let target = record.symlink_target else { return }
    var parent = record.parent
    var depth = 0
    var seen = Set<String>()
    while let ancestor = records.first(where: { $0.id == parent }), ancestor.node != nil {
        guard ancestor.directory, seen.insert(ancestor.id).inserted, depth < 256 else { throw CocoaError(.fileReadCorruptFile) }
        depth += 1; parent = ancestor.parent
    }
    guard safeSymlinkTarget(target, parentDepth: depth) else { throw CocoaError(.fileReadCorruptFile) }
}

// File Provider exposes only ownerExecutable in item flags. The staged contents
// and signed native receiver retain all three negotiated x bits by descriptor.
// Never restore a source POSIX mode: retain safe local rw defaults only.
func applyExecutableBits(_ bits: UInt16?, descriptor: Int32) throws {
    guard let bits else { return }
    var metadata = stat()
    guard bits & ~0o111 == 0, fstat(descriptor, &metadata) == 0,
          metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
          fchmod(descriptor, (metadata.st_mode & 0o664) | mode_t(bits)) == 0 else { throw CocoaError(.fileWriteNoPermission) }
}
