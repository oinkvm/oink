import Foundation
import FileProvider
import Darwin
import OSLog

private let enumerationLog = Logger(subsystem: providerLogSubsystem, category: "enumeration")

// Numeric scopes make recovery verifiable without names, paths or identifiers.
private func enumerationScope(_ identifier: NSFileProviderItemIdentifier) -> Int {
    if identifier == .workingSet { return 1 }
    if identifier == .rootContainer { return 2 }
    if identifier == .trashContainer { return 3 }
    return 4
}
private func enumerationResult(_ operation: String, scope: Int, count: Int = 0,
                               deleted: Int = 0, error: Error? = nil) {
    let code = error.map { ($0 as NSError).code } ?? 0
    enumerationLog.notice("operation=\(operation, privacy: .public) scope=\(scope) result=\(code) count=\(count) deleted=\(deleted)")
}

func makeEnumerator(_ identifier: NSFileProviderItemIdentifier, rootURL: @escaping () throws -> URL = container,
                    signal: @escaping () -> Void = {}) throws -> Enumerator {
    do {
        // NSFileProviderReplicatedExtension.h explicitly requires this Cocoa
        // error when trashing is unsupported. Returning an empty enumerator
        // incorrectly advertises trash support during an expired-anchor scan.
        if identifier == .trashContainer { throw CocoaError(.featureUnsupported) }
        if identifier != .rootContainer && identifier != .workingSet {
            guard try catalogs(in: rootURL()).contains(where: { $0.records.contains { $0.id == identifier.rawValue } }) else {
                throw NSFileProviderError(.noSuchItem)
            }
        }
        enumerationResult("open", scope: enumerationScope(identifier))
        return Enumerator(identifier, rootURL: rootURL, signal: signal)
    } catch {
        enumerationResult("open", scope: enumerationScope(identifier), error: error)
        throw error
    }
}

final class Enumerator: NSObject, NSFileProviderEnumerator {
    let identifier: NSFileProviderItemIdentifier
    private let rootURL: () throws -> URL
    private let lock = NSLock()
    private var initial: CatalogSnapshot?
    private var calls = 0
    private var invalidated = false
    private let directory: (Catalog, Record, DirectoryCursor?) throws -> DirectoryListing
    private let signal: () -> Void
    init(_ identifier: NSFileProviderItemIdentifier, rootURL: @escaping () throws -> URL = container,
         directory: @escaping (Catalog, Record, DirectoryCursor?) throws -> DirectoryListing = { try listDirectory(catalog: $0, record: $1, cursor: $2) },
         signal: @escaping () -> Void = {}) {
        self.identifier = identifier
        self.rootURL = rootURL
        self.directory = directory
        self.signal = signal
    }
    private func count(_ operation: String) {
        calls += 1
        if calls <= 5 || calls % 100 == 0 {
            enumerationLog.notice("operation=\(operation, privacy: .public) scope=\(enumerationScope(self.identifier)) calls=\(self.calls)")
        }
    }
    func invalidate() { lock.withLock { invalidated = true } }
    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        ReportCapture.refresh()
        do {
            if identifier != .rootContainer && identifier != .workingSet,
               let catalog = try catalogs(in: rootURL()).first(where: { $0.records.contains { $0.id == identifier.rawValue } }),
               let record = catalog.records.first(where: { $0.id == identifier.rawValue }), record.directory, record.node != nil {
                guard !isPreparedPlaceholder(record) else { throw NSFileProviderError(.noSuchItem) }
                let cursor: DirectoryCursor?
                if page.rawValue == NSFileProviderPage.initialPageSortedByName as Data || page.rawValue == NSFileProviderPage.initialPageSortedByDate as Data {
                    cursor = nil
                } else {
                    let token = try JSONDecoder().decode(DirectoryPageToken.self, from: page.rawValue)
                    guard token.directory == identifier.rawValue else { throw NSFileProviderError(.pageExpired) }
                    cursor = token.cursor
                }
                // Network and catalog changes run off the callback thread. A
                // delayed directory yields an actual page, never empty completion.
                DispatchQueue.global(qos: .userInitiated).async {
                    do {
                        let listing = try self.directory(catalog, record, cursor)
                        guard !self.lock.withLock({ self.invalidated }) else { throw CocoaError(.userCancelled) }
                        let next = try listing.next.map { NSFileProviderPage(try JSONEncoder().encode(DirectoryPageToken(directory: self.identifier.rawValue, cursor: $0))) }
                        observer.didEnumerate(listing.records.map(Item.init))
                        enumerationResult("items-finished", scope: enumerationScope(self.identifier), count: listing.records.count)
                        observer.finishEnumerating(upTo: next)
                        self.signal()
                    } catch {
                        enumerationResult("items-finished", scope: enumerationScope(self.identifier), error: error)
                        observer.finishEnumeratingWithError(error)
                    }
                }
                return
            }
            let snapshot = try lock.withLock {
                count("items")
                let snapshot = try initial ?? CatalogSnapshot.capture(in: rootURL())
                initial = snapshot
                return snapshot
            }
            let records = snapshot.scoped(to: identifier)
            observer.didEnumerate(records.map(Item.init))
            recordChildrenEnumerated(records, in: try rootURL())
            enumerationResult("items-finished", scope: enumerationScope(identifier), count: records.count)
            observer.finishEnumerating(upTo: nil)
            // The full scan and its anchor are a consistent snapshot. Publications
            // racing that scan still need a subsequent working-set delta. Only
            // signal on this completion event, never from enumerateChanges itself.
            signal()
        } catch {
            enumerationResult("items-finished", scope: enumerationScope(identifier), error: error)
            observer.finishEnumeratingWithError(error)
        }
    }
    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        do {
            let (snapshot, changes) = try lock.withLock {
                count("changes")
                let root = try rootURL()
                let old = try CatalogSnapshot.load(anchor, in: root)
                let snapshot = try CatalogSnapshot.capture(in: root)
                let changes = snapshot.changes(since: old, in: identifier)
                initial = nil
                return (snapshot, changes)
            }
            if !changes.deleted.isEmpty { observer.didDeleteItems(withIdentifiers: changes.deleted) }
            if !changes.updated.isEmpty { observer.didUpdate(changes.updated.map(Item.init)) }
            recordChildrenEnumerated(changes.updated, in: try rootURL())
            enumerationResult("changes-finished", scope: enumerationScope(identifier), count: changes.updated.count,
                              deleted: changes.deleted.count)
            observer.finishEnumeratingChanges(upTo: snapshot.anchor, moreComing: false)
        } catch {
            // The observer discards its old enumeration after expiry; discard our
            // pinned initial page too, even if it reuses this enumerator instance.
            // A subsequent initial page must reflect a new synchronization point.
            if (error as NSError).domain == NSFileProviderErrorDomain,
               (error as NSError).code == NSFileProviderError.syncAnchorExpired.rawValue {
                lock.withLock { initial = nil }
            }
            enumerationResult("changes-finished", scope: enumerationScope(identifier), error: error)
            observer.finishEnumeratingWithError(error)
        }
    }
    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        do {
            let snapshot = try lock.withLock {
                count("anchor")
                let snapshot = try CatalogSnapshot.capture(in: rootURL())
                initial = snapshot
                return snapshot
            }
            // FileProvider synchronously calls enumerateItems inside this completion on macOS.
            // Never invoke system/observer callbacks while holding our state lock.
            enumerationResult("anchor-finished", scope: enumerationScope(identifier), count: snapshot.records.count)
            completionHandler(snapshot.anchor)
        } catch {
            lock.withLock { initial = nil }
            enumerationResult("anchor-finished", scope: enumerationScope(identifier), error: error)
            completionHandler(nil)
        }
    }
}

@objc(OinkFileProvider)
final class OinkFileProvider: NSObject, NSFileProviderReplicatedExtension {
    private let domain: NSFileProviderDomain
    required init(domain: NSFileProviderDomain) { self.domain = domain; super.init(); ReportCapture.refresh() }
    func invalidate() {}
    func item(for identifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest, completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {
        do {
            if identifier == .rootContainer { completionHandler(rootItem(), nil) }
            else if let record = try catalogs().flatMap({ $0.records }).first(where: { $0.id == identifier.rawValue }) { completionHandler(Item(record), nil) }
            else { completionHandler(nil, NSFileProviderError(.noSuchItem)) }
        } catch { completionHandler(nil, error) }
        return Progress(totalUnitCount: 0)
    }
    func fetchContents(for identifier: NSFileProviderItemIdentifier, version requestedVersion: NSFileProviderItemVersion?, request: NSFileProviderRequest, completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {
        fetchContents(for: identifier, progress: downloadProgress(), catalogRoot: container,
                      temporaryRoot: contentTemporaryDirectory, completionHandler: completionHandler)
    }
    // Keep the actual streaming path testable with private fixture roots, without
    // registering a system domain or opening the user's provider cache.
    func fetchContents(for identifier: NSFileProviderItemIdentifier, progress: Progress,
                       catalogRoot: @escaping () throws -> URL, temporaryRoot: @escaping () throws -> URL,
                       completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {
        DispatchQueue.global(qos: .userInitiated).async {
            var temporary: URL?
            do {
                guard let catalog = try catalogs(in: catalogRoot()).first(where: { $0.records.contains { $0.id == identifier.rawValue } }),
                      let record = catalog.records.first(where: { $0.id == identifier.rawValue }), !record.directory, !isPreparedPlaceholder(record) else { throw NSFileProviderError(.noSuchItem) }
                progress.totalUnitCount = record.size.map { Int64(clamping: $0) } ?? -1
                let staging = try temporaryRoot()
                var stagingLease: TemporaryRootLease? = try TemporaryRootLease(staging)
                var url = staging.appendingPathComponent(UUID().uuidString)
                temporary = url
                guard FileManager.default.createFile(atPath: url.path, contents: nil) else { throw CocoaError(.fileWriteUnknown) }
                let output = try FileHandle(forWritingTo: url)
                guard flock(output.fileDescriptor, LOCK_EX) == 0 else { throw CocoaError(.fileWriteUnknown) }
                stagingLease = nil
                _ = stagingLease
                defer { try? output.close() }
                let fd = socket(AF_INET, SOCK_STREAM, 0)
                guard fd >= 0 else { throw NSFileProviderError(.serverUnreachable) }
                defer { close(fd) }
                var timeout = timeval(tv_sec: 1, tv_usec: 0)
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
                let request = try bridgeRequest("read", catalog: catalog, record: record)
                try request.withUnsafeBytes { bytes in
                    var offset = 0
                    while offset < bytes.count {
                        let n = Darwin.write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                        guard n > 0 else { throw NSFileProviderError(.serverUnreachable) }
                        offset += n
                    }
                }
                // Framed stream: 4-byte BE length, bytes. Zero terminates successfully;
                // UInt32.max is failure. EOF without a terminator is never success.
                func readExactly(_ count: Int) throws -> Data {
                    var data = Data(count: count)
                    var lastRead = Date()
                    try data.withUnsafeMutableBytes { bytes in
                        var offset = 0
                        while offset < count {
                            if progress.isCancelled { throw CocoaError(.userCancelled) }
                            let n = Darwin.read(fd, bytes.baseAddress!.advanced(by: offset), count - offset)
                            if n < 0 && (errno == EAGAIN || errno == EINTR) && Date().timeIntervalSince(lastRead) < 310 { continue }
                            guard n > 0 else { throw NSFileProviderError(.serverUnreachable) }
                            offset += n
                            lastRead = Date()
                        }
                    }
                    return data
                }
                var metadata: CurrentMetadata?
                while true {
                    let header = try readExactly(4)
                    let count = header.reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
                    if count == 0 {
                        guard let metadata, metadata.size == UInt64(progress.completedUnitCount) else { throw NSFileProviderError(.cannotSynchronize) }
                        break
                    }
                    if count == UInt32.max - 1 {
                        guard metadata == nil, progress.completedUnitCount == 0 else { throw NSFileProviderError(.cannotSynchronize) }
                        let length = try readExactly(4).reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
                        guard length <= 4096 else { throw NSFileProviderError(.cannotSynchronize) }
                        metadata = try JSONDecoder().decode(CurrentMetadata.self, from: readExactly(Int(length)))
                        progress.totalUnitCount = Int64(clamping: metadata!.size)
                        continue
                    }
                    if count == UInt32.max {
                        switch try readExactly(1).first! {
                        case 1: throw NSFileProviderError(.serverUnreachable)
                        case 2: throw NSFileProviderError(.noSuchItem)
                        case 3: throw NSFileProviderError(.versionNoLongerAvailable)
                        case 4: throw CocoaError(.userCancelled)
                        default: throw NSFileProviderError(.cannotSynchronize)
                        }
                    }
                    guard metadata != nil, count <= 1024 * 1024 else { throw NSFileProviderError(.cannotSynchronize) }
                    let bytes = try readExactly(Int(count))
                    try output.write(contentsOf: bytes)
                    progress.completedUnitCount += Int64(bytes.count)
                    progress.totalUnitCount = max(progress.totalUnitCount, progress.completedUnitCount)
                }
                try output.synchronize()
                guard let metadata else { throw NSFileProviderError(.cannotSynchronize) }
                let current = record.updating(metadata)
                let handoff = url.deletingLastPathComponent().appendingPathComponent("handoff-" + record.id + "-" + url.lastPathComponent)
                try FileManager.default.moveItem(at: url, to: handoff)
                url = handoff
                temporary = handoff
                // The completion transfers ownership to File Provider. It may
                // consume the URL before returning, so a deferred close leaves
                // that consumer racing our exclusive writer lock. Mark handoff
                // before closing to keep cache cleanup from claiming the file.
                try output.close()
                progress.totalUnitCount = progress.completedUnitCount
                progress.fileCompletedCount = 1
                completionHandler(url, Item(current), nil)
            } catch {
                if let temporary { try? FileManager.default.removeItem(at: temporary) }
                completionHandler(nil, nil, error)
            }
        }
        return progress
    }
    func createItem(basedOn itemTemplate: NSFileProviderItem, fields: NSFileProviderItemFields, contents: URL?, options: NSFileProviderCreateItemOptions, request: NSFileProviderRequest, completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        completionHandler(nil, [], false, CocoaError(.fileWriteNoPermission)); return Progress(totalUnitCount: 0)
    }
    func modifyItem(_ item: NSFileProviderItem, baseVersion version: NSFileProviderItemVersion, changedFields: NSFileProviderItemFields, contents: URL?, options: NSFileProviderModifyItemOptions, request: NSFileProviderRequest, completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        completionHandler(nil, [], false, CocoaError(.fileWriteNoPermission)); return Progress(totalUnitCount: 0)
    }
    func deleteItem(identifier: NSFileProviderItemIdentifier, baseVersion version: NSFileProviderItemVersion, options: NSFileProviderDeleteItemOptions, request: NSFileProviderRequest, completionHandler: @escaping (Error?) -> Void) -> Progress {
        completionHandler(CocoaError(.fileWriteNoPermission)); return Progress(totalUnitCount: 0)
    }
    func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest) throws -> NSFileProviderEnumerator {
        try makeEnumerator(containerItemIdentifier, signal: { [domain] in
            guard let manager = NSFileProviderManager(for: domain) else {
                enumerationLog.error("operation=signal scope=1 result=1 count=0")
                return
            }
            Task {
                do {
                    try await manager.signalEnumerator(for: .workingSet)
                    enumerationResult("signal", scope: 1)
                } catch { enumerationResult("signal", scope: 1, error: error) }
            }
        })
    }
}
