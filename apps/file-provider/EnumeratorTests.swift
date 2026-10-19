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
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
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
        for (scope, domain, code) in [
            (NSFileProviderItemIdentifier.trashContainer, NSCocoaErrorDomain, CocoaError.featureUnsupported.rawValue),
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
        let provider = OinkFileProvider(domain: NSFileProviderDomain(identifier: .init("unit-test"), displayName: "Unit Test"))
        let request = NSFileProviderRequest()
        let exported = Item(file)
        var rejected = 0
        _ = provider.createItem(basedOn: exported, fields: [.contents], contents: nil, options: [], request: request) { item, _, _, error in
            precondition(item == nil && (error as NSError?)?.code == CocoaError.fileWriteNoPermission.rawValue)
            rejected += 1
        }
        _ = provider.modifyItem(exported, baseVersion: exported.itemVersion, changedFields: [.contents, .filename, .parentItemIdentifier], contents: nil, options: [], request: request) { item, _, _, error in
            precondition(item == nil && (error as NSError?)?.code == CocoaError.fileWriteNoPermission.rawValue)
            rejected += 1
        }
        _ = provider.deleteItem(identifier: exported.itemIdentifier, baseVersion: exported.itemVersion, options: [], request: request) { error in
            precondition((error as NSError?)?.code == CocoaError.fileWriteNoPermission.rawValue)
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
            let metadata = Data(#"{"schema":1,"size":7,"version":2}"#.utf8)
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
            precondition(item.itemVersion?.contentVersion == Data("2-7-unknown".utf8))
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
        print("Enumerator callbacks: initial race, idle, restart, deletion, expiry/full-scan/delta recovery, container errors, mutation rejection and unlocked fetch handoff passed")
    }
}
