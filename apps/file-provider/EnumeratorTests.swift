#if !DEBUG
#error("File Provider test fixtures require a debug build")
#endif
import Foundation
import FileProvider
import Darwin

private final class Observer: NSObject, NSFileProviderEnumerationObserver, NSFileProviderChangeObserver {
    var items: [NSFileProviderItem] = []
    var deleted: [NSFileProviderItemIdentifier] = []
    var anchor: NSFileProviderSyncAnchor?
    var error: Error?
    var finished = false
    var nextPage: NSFileProviderPage?
    let completion = DispatchSemaphore(value: 0)
    func didEnumerate(_ items: [NSFileProviderItem]) { self.items += items }
    func finishEnumerating(upTo nextPage: NSFileProviderPage?) { self.nextPage = nextPage; finished = true; completion.signal() }
    func finishEnumeratingWithError(_ error: Error) { self.error = error; completion.signal() }
    func didUpdate(_ updatedItems: [NSFileProviderItem]) { items += updatedItems }
    func didDeleteItems(withIdentifiers identifiers: [NSFileProviderItemIdentifier]) { deleted += identifiers }
    func finishEnumeratingChanges(upTo anchor: NSFileProviderSyncAnchor, moreComing: Bool) {
        precondition(!moreComing)
        self.anchor = anchor
        finished = true
    }
}

@main struct EnumeratorTests {
    static func main() throws {
        let old = OinkFileProvider(domain: NSFileProviderDomain(identifier: .init(providerDomain + "-links"), displayName: "Build 48"))
        var refused = false
        do { _ = try old.enumerator(for: .workingSet, request: NSFileProviderRequest()) }
        catch { refused = (error as NSError).code == NSFileProviderError.notAuthenticated.rawValue }
        precondition(refused, "Abandoned build 48 domain must fail before accessing the App Group")

        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        try symlinkFetchCreatesLeaf(in: root.appendingPathComponent("symlink-fetch"))
        try localCreateAndReimport(in: root.appendingPathComponent("local-import"))
        let enumerator = Enumerator(.workingSet, rootURL: { root })
        var start: NSFileProviderSyncAnchor!
        enumerator.currentSyncAnchor { start = $0 }
        precondition(start != nil)
        // macOS FPXEnumerator invokes enumerateItems synchronously from this callback.
        let reentrant = Observer()
        enumerator.currentSyncAnchor { _ in
            enumerator.enumerateItems(for: reentrant, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        }
        precondition(reentrant.finished && reentrant.error == nil)
        let file = Record(id: "test", parent: NSFileProviderItemIdentifier.rootContainer.rawValue, name: "test.txt", directory: false, size: 0, modified: nil, node: 0)
        let catalog = root.appendingPathComponent("test.json")
        // A publication racing initial enumeration must arrive through the subsequent delta.
        try JSONEncoder().encode(Catalog(token: "secret", port: 1, records: [file])).write(to: catalog)
        let initial = Observer()
        enumerator.enumerateItems(for: initial, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        precondition(initial.finished && initial.items.isEmpty && initial.error == nil)
        let added = Observer()
        enumerator.enumerateChanges(for: added, from: start)
        precondition(added.finished && added.items.map { $0.itemIdentifier.rawValue } == ["test"] && added.error == nil)
        var latest = added.anchor!
        // A selected source edit updates this same identity in the working set
        // before content is returned; neither deletion nor a new item is needed.
        let selected = try JSONDecoder().decode(CurrentMetadata.self, from: Data(#"{"schema":1,"size":19,"modified":1700000001234,"version":42}"#.utf8))
        let refreshed = file.updating(selected)
        try JSONEncoder().encode(Catalog(token: "secret", port: 1, records: [refreshed])).write(to: catalog, options: .atomic)
        let changed = Observer()
        Enumerator(.workingSet, rootURL: { root }).enumerateChanges(for: changed, from: latest)
        precondition(changed.finished && changed.error == nil && changed.deleted.isEmpty)
        precondition(changed.items.count == 1 && changed.items[0].itemIdentifier == Item(file).itemIdentifier)
        precondition(changed.items[0].documentSize??.uint64Value == selected.size)
        precondition(changed.items[0].itemVersion?.contentVersion == Item(refreshed).itemVersion.contentVersion)
        precondition(Item(file).itemVersion != Item(refreshed).itemVersion)
        latest = changed.anchor!
        // Recreating the enumerator models an extension restart; no recurring expiration.
        for _ in 0..<100 {
            let idle = Observer()
            Enumerator(.workingSet, rootURL: { root }).enumerateChanges(for: idle, from: latest)
            precondition(idle.finished && idle.items.isEmpty && idle.deleted.isEmpty && idle.anchor == latest && idle.error == nil)
        }
        try FileManager.default.removeItem(at: catalog)
        let deleted = Observer()
        Enumerator(.workingSet, rootURL: { root }).enumerateChanges(for: deleted, from: latest)
        precondition(deleted.finished && deleted.deleted.map { $0.rawValue } == ["test"] && deleted.error == nil)
        latest = deleted.anchor!
        let idle = Observer()
        enumerator.enumerateChanges(for: idle, from: latest)
        precondition(idle.finished && idle.items.isEmpty && idle.deleted.isEmpty && idle.anchor == latest)
        let rejectedSchema = Observer()
        let invalidSchemaAnchor = NSFileProviderSyncAnchor(Data(String(data: latest.rawValue, encoding: .utf8)!.replacingOccurrences(of: "oink-v3-", with: "wrong-v3-").utf8))
        enumerator.enumerateChanges(for: rejectedSchema, from: invalidSchemaAnchor)
        precondition((rejectedSchema.error as NSError?)?.code == NSFileProviderError.syncAnchorExpired.rawValue)
        let incompatibleKey = "oink-v3-" + String(repeating: "0", count: 64)
        let incompatibleAnchor = NSFileProviderSyncAnchor(Data(incompatibleKey.utf8))
        let incompatibleFile = root.appendingPathComponent("anchors").appendingPathComponent(incompatibleKey)
        try JSONEncoder().encode(Catalog(token: "secret", port: 1, records: [file])).write(to: catalog)
        // A malformed anchor is not converted to an empty namespace. Expiry also
        // releases a pinned old initial snapshot on this same enumerator object.
        enumerator.currentSyncAnchor { start = $0 }
        let later = Record(id: "later", parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
                           name: "later.txt", directory: false, size: 1, modified: nil, node: 1)
        try JSONEncoder().encode(Catalog(token: "secret", port: 1, records: [file, later])).write(to: catalog)
        try Data("[{".utf8).write(to: incompatibleFile)
        let incompatible = Observer()
        enumerator.enumerateChanges(for: incompatible, from: incompatibleAnchor)
        let error = incompatible.error as NSError?
        precondition(error?.domain == NSFileProviderErrorDomain && error?.code == NSFileProviderError.syncAnchorExpired.rawValue)
        precondition(!incompatible.finished && incompatible.items.isEmpty && incompatible.deleted.isEmpty && incompatible.anchor == nil)
        precondition(!FileManager.default.fileExists(atPath: incompatibleFile.path))
        let restart = Observer()
        enumerator.enumerateItems(for: restart, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByDate as Data))
        precondition(restart.finished && restart.error == nil && Set(restart.items.map { $0.itemIdentifier.rawValue }) == ["test", "later"])
        // The expired-anchor contract allows starting over with a nil/empty
        // anchor; return the full working set, followed by a valid stable token.
        let fromBeginning = Observer()
        enumerator.enumerateChanges(for: fromBeginning, from: NSFileProviderSyncAnchor(Data()))
        precondition(fromBeginning.finished && fromBeginning.error == nil && fromBeginning.items.count == 2)
        let afterBeginning = Observer()
        Enumerator(.workingSet, rootURL: { root }).enumerateChanges(for: afterBeginning, from: fromBeginning.anchor!)
        precondition(afterBeginning.finished && afterBeginning.error == nil && afterBeginning.items.isEmpty)
        let trash = try makeEnumerator(.trashContainer,rootURL:{root})
        let trashObserver=Observer()
        trash.enumerateItems(for:trashObserver,startingAt:NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        precondition(trashObserver.finished && trashObserver.error == nil)
        for (scope, domain, code) in [
            (NSFileProviderItemIdentifier("missing"), NSFileProviderErrorDomain, NSFileProviderError.noSuchItem.rawValue)
        ] {
            do {
                _ = try makeEnumerator(scope, rootURL: { root })
                preconditionFailure("unsupported/deleted enumerator accepted")
            } catch {
                precondition((error as NSError).domain == domain && (error as NSError).code == code)
            }
        }
        try JSONEncoder().encode(Catalog(token: "secret", port: 1, records: [file])).write(to: catalog)
        enumerator.currentSyncAnchor { start = $0 }
        let freshItems = Observer()
        enumerator.enumerateItems(for: freshItems, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        precondition(freshItems.finished && freshItems.error == nil && freshItems.items.map { $0.itemIdentifier.rawValue } == ["test"])
        let recovered = Observer()
        enumerator.enumerateChanges(for: recovered, from: start)
        precondition(recovered.finished && recovered.error == nil)

        // General expiry recovery, not a stored-schema migration: exercise both
        // system scopes with unknown, pruned, malformed and invalid-shape anchors.
        // Model the SDK sequence and a publication during initial enumeration.
        for scope in [NSFileProviderItemIdentifier.workingSet, .rootContainer] {
            for failure in 0..<4 {
                let key = "oink-v3-" + String(repeating: String(failure + 1), count: 64)
                let badFile = root.appendingPathComponent("anchors").appendingPathComponent(key)
                let badAnchor = failure == 0 ? NSFileProviderSyncAnchor(Data("unknown-anchor".utf8)) : NSFileProviderSyncAnchor(Data(key.utf8))
                if failure == 2 { try Data("[{".utf8).write(to: badFile) }
                if failure == 3 { try Data(#"[{"unexpected":true}]"#.utf8).write(to: badFile) }
                var signals = 0
                let candidate = try makeEnumerator(scope, rootURL: { root }, signal: { signals += 1 })
                let expired = Observer()
                candidate.enumerateChanges(for: expired, from: badAnchor)
                precondition((expired.error as NSError?)?.code == NSFileProviderError.syncAnchorExpired.rawValue)
                precondition(!expired.finished && expired.items.isEmpty && expired.deleted.isEmpty)
                var freshAnchor: NSFileProviderSyncAnchor!
                let full = Observer()
                candidate.currentSyncAnchor { anchor in
                    freshAnchor = anchor
                    candidate.enumerateItems(for: full, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByDate as Data))
                }
                precondition(freshAnchor != nil && full.finished && full.error == nil && signals == 1)
                precondition(full.items.map { $0.itemIdentifier.rawValue } == ["test"])
                try JSONEncoder().encode(Catalog(token: "secret", port: 1, records: [file, later])).write(to: catalog)
                let delta = Observer()
                candidate.enumerateChanges(for: delta, from: freshAnchor)
                precondition(delta.finished && delta.error == nil && delta.items.map { $0.itemIdentifier.rawValue } == ["later"])
                precondition(signals == 1, "change enumeration must not signal itself in a loop")
                let restarted = Observer()
                try makeEnumerator(scope, rootURL: { root }).enumerateChanges(for: restarted, from: delta.anchor!)
                precondition(restarted.finished && restarted.error == nil && restarted.items.isEmpty && restarted.deleted.isEmpty)
                try JSONEncoder().encode(Catalog(token: "secret", port: 1, records: [file])).write(to: catalog)
            }
        }

        // A failed fresh anchor must not leave a usable-looking stale initial
        // snapshot. Live catalog failures stay errors; only anchor caches discard.
        let damagedLive = try makeEnumerator(.workingSet, rootURL: { root })
        damagedLive.currentSyncAnchor { precondition($0 != nil) }
        try Data("invalid".utf8).write(to: catalog)
        damagedLive.currentSyncAnchor { precondition($0 == nil) }
        let damagedItems = Observer()
        damagedLive.enumerateItems(for: damagedItems, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        precondition(damagedItems.error != nil && !damagedItems.finished && damagedItems.items.isEmpty)
        precondition(FileManager.default.fileExists(atPath: catalog.path))
        try JSONEncoder().encode(Catalog(token: "secret", port: 1, records: [file])).write(to: catalog)

        // A pre-materialized empty parent is filled with exactly one batch's
        // children; cancellation and restart reconciliation use ordinary WS deltas.
        let spareKey = String(repeating: "a", count: 64)
        let spareURL = root.appendingPathComponent(spareKey + ".json")
        let marker = root.appendingPathComponent(spareKey + ".enumerated")
        let emptyParent = Record(id: spareKey + "-root", parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
            name: spareKey, directory: true, size: nil, modified: nil, node: nil, version: 1, children: 0)
        try JSONEncoder().encode(Catalog(token: "spare", port: 1, records: [emptyParent])).write(to: spareURL)
        let beforeFill = try CatalogSnapshot.capture(in: root)
        let emptyObserver = Observer()
        Enumerator(.workingSet, rootURL: { root }).enumerateItems(for: emptyObserver, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        precondition(!FileManager.default.fileExists(atPath: marker.path))
        let fullParent = Record(id: emptyParent.id, parent: emptyParent.parent, name: spareKey,
            directory: true, size: nil, modified: nil, node: nil, version: 1, children: 2)
        let childA = Record(id: spareKey + "-1", parent: fullParent.id, name: "a", directory: false, size: 1, modified: nil, node: 1, version: 1)
        let childB = Record(id: spareKey + "-2", parent: fullParent.id, name: "b", directory: false, size: 2, modified: nil, node: 2, version: 1)
        try JSONEncoder().encode(Catalog(token: "spare", port: 1, records: [fullParent, childA, childB])).write(to: spareURL)
        recordChildrenEnumerated([childA], in: root)
        precondition(!FileManager.default.fileExists(atPath: marker.path))
        let filled = Observer()
        Enumerator(.workingSet, rootURL: { root }).enumerateChanges(for: filled, from: beforeFill.anchor)
        precondition(filled.finished && filled.error == nil && filled.items.map { $0.itemIdentifier.rawValue } == [fullParent.id, childA.id, childB.id])
        let firstObservation = try Data(contentsOf: marker)
        recordChildrenEnumerated([childA, childB], in: root)
        let repeatedObservation = try Data(contentsOf: marker)
        precondition(repeatedObservation == firstObservation)
        precondition(Item(emptyParent).itemVersion.metadataVersion != Item(fullParent).itemVersion.metadataVersion)
        try FileManager.default.removeItem(at: spareURL)
        let retired = Observer()
        Enumerator(.workingSet, rootURL: { root }).enumerateChanges(for: retired, from: filled.anchor!)
        precondition(retired.finished && retired.error == nil && Set(retired.deleted.map { $0.rawValue }) == Set([fullParent.id, childA.id, childB.id]))
        // Reconciliation of an unused spare deletes the empty parent as well.
        try JSONEncoder().encode(Catalog(token: "spare", port: 1, records: [emptyParent])).write(to: spareURL)
        let stale = try CatalogSnapshot.capture(in: root)
        try FileManager.default.removeItem(at: spareURL)
        let reconciled = Observer()
        Enumerator(.workingSet, rootURL: { root }).enumerateChanges(for: reconciled, from: stale.anchor)
        precondition(reconciled.finished && reconciled.deleted.map { $0.rawValue } == [emptyParent.id])

        // A native directory callback returns before network completion. Causal
        // gates prove that the pending view was not reported as an empty folder.
        let directory = Record(id: "batch-7", parent: NSFileProviderItemIdentifier.rootContainer.rawValue, name: "folder", directory: true, size: nil, modified: nil, node: 7)
        try JSONEncoder().encode(Catalog(token: "secret", port: 1, records: [directory])).write(to: catalog)
        let entered = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let firstChild = Record(id: "batch-8", parent: "batch-7", name: "first", directory: false, size: 2, modified: nil, node: 8)
        let secondChild = Record(id: "batch-9", parent: "batch-7", name: "second", directory: false, size: nil, modified: nil, node: 9)
        let paged = Enumerator(.init("batch-7"), rootURL: { root }, directory: { _, record, cursor in
            precondition(record.id == "batch-7")
            if cursor == nil {
                entered.signal(); release.wait()
                return DirectoryListing(records: [firstChild], next: DirectoryCursor(version: 42, offset: 1), version: 42)
            }
            precondition(cursor == DirectoryCursor(version: 42, offset: 1))
            return DirectoryListing(records: [secondChild], next: nil, version: 42)
        })
        let firstPage = Observer()
        paged.enumerateItems(for: firstPage, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        precondition(entered.wait(timeout: .now() + 15) == .success) // hang watchdog only
        precondition(!firstPage.finished && firstPage.items.isEmpty)
        release.signal()
        precondition(firstPage.completion.wait(timeout: .now() + 15) == .success)
        precondition(firstPage.finished && firstPage.items.map { $0.itemIdentifier.rawValue } == ["batch-8"])
        let nextPage = firstPage.nextPage!
        let next = Observer()
        paged.enumerateItems(for: next, startingAt: nextPage)
        precondition(next.completion.wait(timeout: .now() + 15) == .success)
        precondition(next.finished && next.nextPage == nil && next.items.map { $0.itemIdentifier.rawValue } == ["batch-9"])
        let wrong = Observer()
        let wrongPage = NSFileProviderPage(try JSONEncoder().encode(DirectoryPageToken(directory: "another", cursor: DirectoryCursor(version: 42, offset: 1))))
        paged.enumerateItems(for: wrong, startingAt: wrongPage)
        precondition((wrong.error as NSError?)?.code == NSFileProviderError.pageExpired.rawValue)

        // Writable export declarations must not create a remote mutation API. Exercise
        // the real callbacks without registering a system domain or touching a desktop.
        let provider = OinkFileProvider(domain: NSFileProviderDomain(identifier: .init("unit-test"), displayName: "Unit Test"),root:root)
        let request = NSFileProviderRequest()
        let missing = NSFileProviderItemIdentifier("missing-item")
        _ = provider.item(for: missing, request: request) { item, error in
            precondition(item == nil)
            let native = error! as NSError
            precondition(native.domain == NSFileProviderErrorDomain && native.code == NSFileProviderError.noSuchItem.rawValue)
            precondition(native.userInfo[NSFileProviderErrorNonExistentItemIdentifierKey] as? String == missing.rawValue)
        }
        let exported = Item(file)
        var rejected = 0
        _ = provider.createItem(basedOn: exported, fields: [.contents], contents: nil, options: [], request: request) { item, _, _, error in
            precondition(item == nil && (error as NSError?)?.code == CocoaError.fileWriteInvalidFileName.rawValue)
            rejected += 1
        }
        _ = provider.modifyItem(exported, baseVersion: exported.itemVersion, changedFields: [.contents, .filename, .parentItemIdentifier], contents: nil, options: [], request: request) { item, _, _, error in
            precondition(item == nil && (error as NSError?)?.code == NSFileProviderError.noSuchItem.rawValue)
            rejected += 1
        }
        _ = provider.deleteItem(identifier: exported.itemIdentifier, baseVersion: exported.itemVersion, options: [], request: request) { error in
            precondition(error == nil)
            rejected += 1
        }
        precondition(rejected == 3)

        // Exercise the real framed fetch and inspect ownership synchronously from
        // its completion: the daemon may consume the URL before this call returns.
        let listener = socket(AF_INET, SOCK_STREAM, 0)
        precondition(listener >= 0)
        defer { close(listener) }
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_addr.s_addr = inet_addr("127.0.0.1")
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(listener, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
        }
        precondition(bound == 0 && listen(listener, 1) == 0)
        var addressSize = socklen_t(MemoryLayout<sockaddr_in>.size)
        precondition(withUnsafeMutablePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(listener, $0, &addressSize) }
        } == 0)
        let fetchRoot = root.appendingPathComponent("fetch")
        let staging = root.appendingPathComponent("staging")
        try FileManager.default.createDirectory(at: fetchRoot, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        let fetched = Record(id: "fetch-file", parent: "fetch-root", name: "file.bin", directory: false,
                             size: 7, modified: nil, node: 1)
        try JSONEncoder().encode(Catalog(token: "unit-test", port: UInt16(bigEndian: address.sin_port), records: [fetched]))
            .write(to: fetchRoot.appendingPathComponent("catalog.json"))
        DispatchQueue.global().async {
            let client = accept(listener, nil, nil)
            precondition(client >= 0)
            defer { close(client) }
            var request = [UInt8](repeating: 0, count: 4096)
            precondition(Darwin.read(client, &request, request.count) > 0)
            let metadata = Data(#"{"schema":1,"size":7,"version":2,"exec_bits":73}"#.utf8)
            var response = Data()
            for bytes in [Data([255, 255, 255, 254]), withUnsafeBytes(of: UInt32(metadata.count).bigEndian, { Data($0) }),
                          metadata, Data([0, 0, 0, 7]), Data("content".utf8), Data([0, 0, 0, 0])] {
                response.append(bytes)
            }
            response.withUnsafeBytes { bytes in
                var offset = 0
                while offset < bytes.count {
                    let count = Darwin.write(client, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                    precondition(count > 0)
                    offset += count
                }
            }
        }
        let completed = DispatchSemaphore(value: 0)
        var handoffUnlocked = false
        var fetchError: Error?
        let fetchProgress = downloadProgress()
        _ = provider.fetchContents(for: .init(fetched.id), progress: fetchProgress,
                                   catalogRoot: { fetchRoot }, temporaryRoot: { staging }) { url, item, error in
            defer { completed.signal() }
            fetchError = error
            guard let url, let item else { return }
            precondition(url.lastPathComponent.hasPrefix("handoff-" + fetched.id + "-"))
            precondition(item.documentSize == 7)
            precondition(item.itemVersion?.contentVersion == Data("2-7-unknown-exec:73".utf8))
            var mode = stat(); precondition(lstat(url.path, &mode) == 0)
            precondition(mode.st_mode & 0o111 == 0o111 && mode.st_mode & 0o7002 == 0)
            precondition(item.fileSystemFlags?.contains(.userExecutable) == true)
            precondition((try? Data(contentsOf: url)) == Data("content".utf8))
            // A fresh open-description lock conflicts even within this process.
            // Success proves the extension released its writer before publishing.
            let reader = Darwin.open(url.path, O_RDONLY | O_SHLOCK | O_NONBLOCK)
            handoffUnlocked = reader >= 0
            if reader >= 0 { close(reader) }
            precondition(fetchProgress.fileCompletedCount == 1 && fetchProgress.completedUnitCount == 7)
            // Releasing the writer must not make handoff data eligible for clear:
            // the daemon owns it until that provider item is evicted.
            let inspected = try! inspectTemporary(in: staging, clear: true, evicted: [])
            precondition(inspected.kept > 0 && inspected.removed == 0)
            precondition(FileManager.default.fileExists(atPath: url.path))
        }
        precondition(completed.wait(timeout: .now() + 15) == .success)
        precondition(fetchError == nil)
        precondition(handoffUnlocked, "fetchContents published a handoff still locked by its writer")
        for gone in [false, true] {
            try editDuringFetch(provider, listener: listener, port: UInt16(bigEndian: address.sin_port),
                                root: root.appendingPathComponent(gone ? "gone-race" : "handoff-race"), gone: gone)
        }
        for matches in [false, true] {
            try requestedFetchVersion(provider, listener: listener, port: UInt16(bigEndian: address.sin_port),
                                      root: root.appendingPathComponent(matches ? "matching-version" : "old-version"), matches: matches)
        }
        for scenario in ["recover", "exhausted", "explicit", "edited", "cancelled", "gone", "denied"] {
            try supersededFetch(provider, listener: listener, port: UInt16(bigEndian: address.sin_port),
                                root: root.appendingPathComponent("superseded-" + scenario), scenario: scenario)
        }
        print("Enumerator callbacks: initial race, idle, restart, deletion, expiry/full-scan/delta recovery, container errors, local import/reimport, mutation boundaries, exact versions, source-change restarts and unlocked fetch handoff passed")
    }
    static func localCreateAndReimport(in root: URL) throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let parent = Record(id: "import-root", parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
                            name: "received", directory: true, size: nil, modified: nil, node: nil)
        let original = Record(id: "import-1", parent: parent.id, name: "document.txt", directory: false,
                              size: 3, modified: nil, node: 1, version: 1)
        try JSONEncoder().encode(Catalog(token: "unused", port: 0, records: [parent, original], received_at: 1))
            .write(to: root.appendingPathComponent("import.json"))
        let provider = OinkFileProvider(domain: NSFileProviderDomain(identifier: .init(providerDomain), displayName: "Test"), root: root)
        let bytes = root.appendingPathComponent("system-content")
        try Data("local edit".utf8).write(to: bytes)
        func create(_ template: Record, _ options: NSFileProviderCreateItemOptions = [], _ content: URL? = bytes) throws -> Record {
            var result: Record?; var failure: Error?
            _ = provider.createItem(basedOn: Item(template), fields: [.filename, .parentItemIdentifier, .contents],
                contents: content, options: options, request: NSFileProviderRequest()) { item, pending, fetch, error in
                precondition(!fetch && pending.isEmpty)
                result = (item as? Item)?.record; failure = error
            }
            if let failure = failure { throw failure }
            return result!
        }
        let template = Record(id: "os-stable-creation", parent: parent.id, name: ".save.tmp", directory: false,
                              size: 999, modified: nil, node: nil)
        let created = try assertCacheMutation { try create(template) }
        precondition(created.size == 10 && created.keep_local == true && created.node == nil)
        precondition(created.id != template.id && created.id.hasPrefix("import-local-"))
        let replay = try create(template)
        precondition(replay == created, "crash replay created a duplicate or new content version")
        let snapshot = try CatalogSnapshot.capture(in: root)
        precondition(snapshot.records.contains { $0.id == created.id }, "overlay-only item missing after restart")
        precondition(Item(created).capabilities.contains(.allowsRenaming))
        try receivedDelete([original.id], in: root)
        let renamed = Record(id: created.id, parent: parent.id, name: original.name, directory: false,
                             size: created.size, modified: nil, node: nil)
        let final = try receivedModify(Item(renamed), fields: [.filename], contents: nil, in: root)
        precondition(final.id == created.id && final.name == original.name)
        do { _ = try create(original, [.deletionConflicted]); preconditionFailure("collision overwrote an unrelated local replacement") } catch {}
        try receivedDelete([final.id], in: root)
        let recovered = try create(original, [.deletionConflicted])
        precondition(recovered.id == original.id && recovered.size == 10 && recovered.keep_local == true)
        let entry = try readReceived("import", in: root)[original.id]!
        precondition(entry.modified_locally == true && !entry.deleted && entry.materialized)
        do { try receivedBegin(original, in: root); preconditionFailure("reimport reattached source") } catch {}
        let known = try create(original, [.mayAlreadyExist], nil)
        precondition(known == recovered)
        let unknown = Record(id: "unknown", parent: parent.id, name: "dataless", directory: false, size: 0, modified: nil, node: nil)
        do { _ = try create(unknown, [.mayAlreadyExist], nil); preconditionFailure("unknown dataless import was accepted as empty") } catch {}
        let protected = try receivedProtected("import", in: root)
        precondition(protected)
        // Local children of a linked directory accompany the final source page.
        let directory = Record(id: "import-2", parent: parent.id, name: "folder", directory: true,
                               size: nil, modified: nil, node: 2, children: 1)
        let sourceChild = Record(id: "import-3", parent: directory.id, name: "remote", directory: false,
                                 size: 3, modified: nil, node: 3)
        try JSONEncoder().encode(Catalog(token: "unused", port: 0, records: [parent, original, directory, sourceChild], received_at: 1))
            .write(to: root.appendingPathComponent("import.json"), options: .atomic)
        let childTemplate = Record(id: "os-local-child", parent: directory.id, name: "local", directory: false,
                                   size: nil, modified: nil, node: nil)
        let localChild = try create(childTemplate)
        try receivedDelete([sourceChild.id], in: root)
        let sourcePage = DirectoryListing(records: [sourceChild], next: nil, version: 1)
        let page = try receivedDirectoryListing(sourcePage, directory: directory, in: root)
        precondition(page.records == [localChild], "old directory page revived deletion or hid local import")
        let enumerated = Observer()
        let e = Enumerator(.init(directory.id), rootURL: { root }, directory: { _, _, _ in sourcePage })
        e.enumerateItems(for: enumerated, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        precondition(enumerated.completion.wait(timeout: .now() + 10) == .success)
        precondition(enumerated.error == nil && enumerated.items.map { $0.itemIdentifier.rawValue } == [localChild.id])
        let gone = Observer()
        let deletedSource = Enumerator(.init(directory.id), rootURL: { root }, directory: { _, _, _ in throw NSFileProviderError(.noSuchItem) })
        deletedSource.enumerateItems(for: gone, startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        precondition(gone.completion.wait(timeout: .now() + 10) == .success)
        precondition((gone.error as NSError?)?.code == NSFileProviderError.versionNoLongerAvailable.rawValue,
                     "source deletion could remove local descendants")
        // Pure local directories are user items, including empty ones.
        let emptyTemplate = Record(id: "os-empty-directory", parent: parent.id, name: "empty-local", directory: true,
                                   size: nil, modified: nil, node: nil)
        let empty = try create(emptyTemplate, [], nil)
        let localSnapshot = try CatalogSnapshot.capture(in: root)
        precondition(localSnapshot.records.contains { $0.id == empty.id })
        let replayFolder = try create(emptyTemplate, [], nil)
        precondition(replayFolder == empty)
        // Failed journaling cannot acknowledge an import or change the OS-owned bytes.
        let journal = root.appendingPathComponent("import.received")
        let saved = try Data(contentsOf: journal)
        try FileManager.default.removeItem(at: journal)
        try FileManager.default.createDirectory(at: journal, withIntermediateDirectories: false)
        let failed = Record(id: "os-failed-import", parent: parent.id, name: "failed", directory: false,
                            size: nil, modified: nil, node: nil)
        do { _ = try create(failed); preconditionFailure("failed journal acknowledged import") } catch {}
        try FileManager.default.removeItem(at: journal)
        try saved.write(to: journal)
        let retainedBytes = try Data(contentsOf: bytes)
        precondition(retainedBytes == Data("local edit".utf8), "provider changed system-owned import contents")
    }

    static func requestedFetchVersion(_ provider: OinkFileProvider, listener: Int32, port: UInt16,
                                      root: URL, matches: Bool) throws {
        let staging = root.appendingPathComponent("staging")
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        let record = Record(id: "version-1", parent: "version-root", name: "file", directory: false,
                            size: 7, modified: nil, node: 1, version: 1)
        try JSONEncoder().encode(Catalog(token: "unit-test", port: port, records: [record], received_at: 1))
            .write(to: root.appendingPathComponent("version.json"))
        DispatchQueue.global().async {
            let client = accept(listener, nil, nil)
            precondition(client >= 0)
            defer { close(client) }
            var noSigPipe: Int32 = 1
            setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
            var request = [UInt8](repeating: 0, count: 4096)
            precondition(Darwin.read(client, &request, request.count) > 0)
            let metadata = Data(#"{"schema":1,"size":7,"version":2}"#.utf8)
            var response = Data()
            for frame in [Data([255, 255, 255, 254]), withUnsafeBytes(of: UInt32(metadata.count).bigEndian, { Data($0) }),
                          metadata, Data([0, 0, 0, 7]), Data("content".utf8), Data([0, 0, 0, 0])] { response.append(frame) }
            response.withUnsafeBytes { bytes in
                var offset = 0
                while offset < bytes.count {
                    let count = Darwin.write(client, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                    if count <= 0 { return }; offset += count
                }
            }
        }
        let completed = DispatchSemaphore(value: 0)
        let requested = NSFileProviderItemVersion(contentVersion: Data((matches ? "2-7-unknown" : "1-7-unknown").utf8),
                                                  metadataVersion: Data("independent-metadata".utf8))
        let progress = downloadProgress()
        _ = provider.fetchContents(for: .init(record.id), progress: progress, requestedVersion: requested,
                                   catalogRoot: { root }, temporaryRoot: { staging }) { url, item, error in
            defer { completed.signal() }
            if matches {
                precondition(error == nil && url != nil && item?.itemVersion?.contentVersion == requested.contentVersion)
                precondition((try? Data(contentsOf: url!)) == Data("content".utf8))
            } else {
                precondition(url == nil && item == nil, "explicit old version silently received current source contents")
                assertFullFetchVersionError(error)
                precondition(progress.completedUnitCount == 0, "requested version must be checked before body bytes")
            }
        }
        precondition(completed.wait(timeout: .now() + 15) == .success)
        let entry = try readReceived("version", in: root)[record.id]!
        precondition(entry.handed_off == matches && entry.fetching == 0)
        if !matches {
            let leftovers = try FileManager.default.contentsOfDirectory(at: staging, includingPropertiesForKeys: nil)
            precondition(!leftovers.contains { $0.lastPathComponent.hasPrefix("handoff-") })
        }
    }

    static func editDuringFetch(_ provider: OinkFileProvider, listener: Int32, port: UInt16,
                               root: URL, gone: Bool) throws {
        let staging = root.appendingPathComponent("staging")
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        let parent = Record(id: "race-root", parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
                            name: "batch", directory: true, size: nil, modified: nil, node: nil)
        let record = Record(id: "race-1", parent: parent.id, name: "file", directory: false,
                            size: 7, modified: nil, node: 1, version: 1)
        try JSONEncoder().encode(Catalog(token: "unit-test", port: port, records: [parent, record], received_at: 1))
            .write(to: root.appendingPathComponent("race.json"))
        let local = root.appendingPathComponent("local-content")
        try Data("edited".utf8).write(to: local)
        DispatchQueue.global().async {
            let client = accept(listener, nil, nil)
            precondition(client >= 0)
            defer { close(client) }
            var request = [UInt8](repeating: 0, count: 4096)
            precondition(Darwin.read(client, &request, request.count) > 0)
            let edited = Record(id: record.id, parent: record.parent, name: record.name, directory: false,
                                size: 6, modified: nil, node: 1, version: 1)
            _ = try! receivedModify(Item(edited), fields: [.contents], contents: local, in: root)
            let metadata = Data(#"{"schema":1,"size":7,"version":2}"#.utf8)
            var response = Data()
            let frames = gone ? [Data([255, 255, 255, 255, 2])] :
                [Data([255, 255, 255, 254]), withUnsafeBytes(of: UInt32(metadata.count).bigEndian, { Data($0) }),
                 metadata, Data([0, 0, 0, 7]), Data("content".utf8), Data([0, 0, 0, 0])]
            for frame in frames { response.append(frame) }
            response.withUnsafeBytes { bytes in
                var offset = 0
                while offset < bytes.count {
                    let count = Darwin.write(client, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                    precondition(count > 0); offset += count
                }
            }
        }
        let completed = DispatchSemaphore(value: 0)
        _ = provider.fetchContents(for: .init(record.id), progress: downloadProgress(),
                                   catalogRoot: { root }, temporaryRoot: { staging }) { url, item, error in
            precondition(url == nil && item == nil)
            assertFullFetchVersionError(error)
            completed.signal()
        }
        precondition(completed.wait(timeout: .now() + 15) == .success)
        let state = try readReceived("race", in: root)[record.id]!
        precondition(state.modified_locally == true && state.record.keep_local == true)
        precondition(state.record.size == 6 && state.fetching == 0)
        precondition(try! Data(contentsOf: local) == Data("edited".utf8))
        let temporary = try FileManager.default.contentsOfDirectory(at: staging, includingPropertiesForKeys: nil)
        precondition(!temporary.contains { $0.lastPathComponent.hasPrefix("handoff-") })
    }

    static func assertFullFetchVersionError(_ error: Error?) {
        let native = error! as NSError
        precondition(native.domain == NSCocoaErrorDomain && native.code == CocoaError.fileReadUnknown.rawValue)
        let underlying = native.userInfo[NSUnderlyingErrorKey] as! NSError
        precondition(underlying.domain == NSFileProviderErrorDomain && underlying.code == NSFileProviderError.versionNoLongerAvailable.rawValue)
    }

    static func supersededFetch(_ provider: OinkFileProvider, listener: Int32, port: UInt16,
                                root: URL, scenario: String) throws {
        let staging = root.appendingPathComponent("staging")
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        let record = Record(id: "restart-1", parent: "restart-root", name: "file", directory: false,
                            size: 7, modified: nil, node: 1, version: 1)
        let parent = Record(id: record.parent, parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
                            name: "batch", directory: true, size: nil, modified: nil, node: nil)
        try JSONEncoder().encode(Catalog(token: "unit-test", port: port, records: [parent, record], received_at: 1))
            .write(to: root.appendingPathComponent("restart.json"))
        let progress = downloadProgress()
        let attempts = scenario == "recover" ? 2 : scenario == "exhausted" ? 3 : 1
        let served = DispatchSemaphore(value: 0)
        DispatchQueue.global().async {
            defer { served.signal() }
            for attempt in 0..<attempts {
                let client = accept(listener, nil, nil)
                precondition(client >= 0)
                defer { close(client) }
                var noSigPipe: Int32 = 1
                setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
                var request = [UInt8](repeating: 0, count: 4096)
                precondition(Darwin.read(client, &request, request.count) > 0)
                let metadata = Data("{\"schema\":1,\"size\":7,\"version\":\(attempt + 1)}".utf8)
                let success = scenario == "recover" && attempt == 1
                let prefix = success ? Data("content".utf8) : Data("old".utf8)
                var response = Data()
                for frame in [Data([255, 255, 255, 254]), withUnsafeBytes(of: UInt32(metadata.count).bigEndian, { Data($0) }),
                              metadata, withUnsafeBytes(of: UInt32(prefix.count).bigEndian, { Data($0) }), prefix] { response.append(frame) }
                if scenario == "edited" {
                    let local = root.appendingPathComponent("local-edit")
                    try! Data("edited!".utf8).write(to: local)
                    _ = try! receivedModify(Item(record), fields: [.contents], contents: local, in: root)
                }
                if scenario == "cancelled" { progress.cancel() }
                response.append(success ? Data([0, 0, 0, 0]) : Data([255, 255, 255, 255, scenario == "gone" ? 2 : scenario == "denied" ? 6 : 3]))
                response.withUnsafeBytes { bytes in
                    var offset = 0
                    while offset < bytes.count {
                        let count = Darwin.write(client, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                        if count <= 0 { return }; offset += count
                    }
                }
            }
        }
        let completed = DispatchSemaphore(value: 0)
        let requested = scenario == "explicit" ? Item(record).itemVersion : nil
        _ = provider.fetchContents(for: .init(record.id), progress: progress, requestedVersion: requested,
                                   catalogRoot: { root }, temporaryRoot: { staging }) { url, item, error in
            defer { completed.signal() }
            if scenario == "recover" {
                precondition(error == nil && url != nil && item?.itemVersion?.contentVersion == Data("2-7-unknown".utf8))
                precondition(try! Data(contentsOf: url!) == Data("content".utf8), "mixed versions escaped the failed attempt")
                precondition(progress.completedUnitCount == 7 && progress.fileCompletedCount == 1)
            } else {
                precondition(url == nil && item == nil)
                if scenario == "cancelled" {
                    precondition((error as NSError?)?.code == CocoaError.userCancelled.rawValue)
                } else if scenario == "denied" {
                    let native = error! as NSError
                    precondition(native.domain == NSCocoaErrorDomain && native.code == CocoaError.fileReadNoPermission.rawValue,
                                 "bridge permission denial was reported as a connection or synchronization failure")
                } else if scenario == "gone" {
                    let native = error! as NSError
                    precondition(native.code == NSFileProviderError.noSuchItem.rawValue)
                    precondition(native.userInfo[NSFileProviderErrorNonExistentItemIdentifierKey] as? String == record.id)
                } else { assertFullFetchVersionError(error) }
            }
        }
        precondition(completed.wait(timeout: .now() + 15) == .success)
        precondition(served.wait(timeout: .now() + 15) == .success)
        let entry = try readReceived("restart", in: root)[record.id]!
        precondition(entry.fetching == 0)
        precondition(entry.handed_off == (scenario == "recover" || scenario == "edited"))
        if scenario == "edited" {
            precondition(entry.modified_locally == true && entry.record.keep_local == true)
            precondition(try! Data(contentsOf: root.appendingPathComponent("local-edit")) == Data("edited!".utf8))
        }
        let files = try FileManager.default.contentsOfDirectory(at: staging, includingPropertiesForKeys: nil)
            .filter { !$0.lastPathComponent.hasPrefix(".") }
        precondition(files.count == (scenario == "recover" ? 1 : 0), "failed attempt left partial bytes behind")
        // No unconsumed retry should remain queued on the shared test listener.
        var poller = pollfd(fd: listener, events: Int16(POLLIN), revents: 0)
        precondition(poll(&poller, 1, 0) == 0)
    }
    static func symlinkFetchCreatesLeaf(in root: URL) throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let staging = root.appendingPathComponent("staging")
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        let tree = Record(id: "link-1", parent: "link-root", name: "Fixture.app", directory: true,
                          size: nil, modified: nil, node: 1, version: 1)
        let link = Record(id: "link-2", parent: tree.id, name: "current", directory: false,
                          size: 7, modified: nil, node: 2, version: 7, symlink_target: "missing")
        try JSONEncoder().encode(Catalog(token: "unused", port: 0, records: [tree, link]))
            .write(to: root.appendingPathComponent("link.json"))
        let provider = OinkFileProvider(domain: NSFileProviderDomain(identifier: .init("unit-test"), displayName: "Unit Test"), root: root)
        let done = DispatchSemaphore(value: 0)
        var failure: Error?; var returned: URL?
        let progress = downloadProgress()
        _ = provider.fetchContents(for: .init(link.id), progress: progress, catalogRoot: { root }, temporaryRoot: { staging }) { url, item, error in
            failure = error; returned = url
            precondition(item?.contentType == .symbolicLink && item?.symlinkTargetPath == "missing")
            done.signal()
        }
        precondition(done.wait(timeout: .now() + 15) == .success)
        precondition(failure == nil && returned != nil && progress.fileCompletedCount == 1)
        let target = try FileManager.default.destinationOfSymbolicLink(atPath: returned!.path)
        precondition(target == "missing", "receiver must not need a live target or a content socket")
        _ = try inspectTemporary(in: staging, clear: true, evicted: [])
        precondition((try? FileManager.default.destinationOfSymbolicLink(atPath: returned!.path)) == "missing")
        _ = try inspectTemporary(in: staging, clear: true, evicted: [link.id])
        var metadata = stat()
        precondition(lstat(returned!.path, &metadata) != 0)
    }

}
