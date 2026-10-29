#if !DEBUG
#error("File Provider test fixtures require a debug build")
#endif
import Foundation
import FileProvider
import Darwin

private final class DisconnectedRemovalDomain: NSFileProviderDomain {
    override var isDisconnected: Bool { true }
    override var userEnabled: Bool { false }
}

private final class RetirementDomain: ProviderItemDomain {
    enum Failure: Error { case unrelatedDomainWork }
    var signals = 0
    var lookups = 0
    var signalFailure: Error?
    var resolve: (NSFileProviderItemIdentifier) throws -> URL = { _ in throw NSFileProviderError(.noSuchItem) }
    func signalEnumerator(for identifier: NSFileProviderItemIdentifier) async throws {
        precondition(identifier == .workingSet)
        signals += 1
        if let signalFailure { throw signalFailure }
    }
    // The retired batch can be gone while an unrelated upload prevents this
    // testing-only barrier from completing. Retirement must not depend on it.
    func waitForStabilization() async throws { throw Failure.unrelatedDomainWork }
    func getUserVisibleURL(for identifier: NSFileProviderItemIdentifier) async throws -> URL {
        lookups += 1
        return try resolve(identifier)
    }
}

@main
struct Tests {
    static func placeholderRetirementIsItemScoped() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let parent = root.appendingPathComponent(".Oink-prepared-batch.noindex")
        func ledger(_ parent: URL?) throws -> PlaceholderLedger {
            let object: [String: Any] = ["schema": 1, "parent": parent?.path as Any? ?? NSNull(), "urls": []]
            return try JSONDecoder().decode(PlaceholderLedger.self, from: JSONSerialization.data(withJSONObject: object))
        }
        let retired = RetirementDomain()
        try await retirePlaceholder(batch: "batch", ledger: ledger(parent), manager: retired,
                                    pause: { preconditionFailure("already retired item waited") })
        precondition(retired.signals == 1 && retired.lookups == 1)

        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        for _ in 0..<3 {
            do {
                try await retirePlaceholder(batch: "batch", ledger: ledger(parent), manager: retired,
                                            pause: { throw CancellationError() })
                preconditionFailure("identity deletion bypassed surviving namespace")
            } catch { precondition(error is CancellationError) }
        }
        try FileManager.default.removeItem(at: parent)
        try await retirePlaceholder(batch: "batch", ledger: ledger(parent), manager: retired,
                                    pause: { preconditionFailure("retirement retry waited after removal") })
        precondition(retired.signals == 5 && retired.lookups == 5)

        // Signal ACK precedes asynchronous creation. The old path is absent,
        // but the item still resolves (possibly under its activation name).
        // First observe that identity disappear, then its actual disk path.
        let late = root.appendingPathComponent("renamed-parent")
        var phase = 0
        let pending = RetirementDomain()
        pending.resolve = { identifier in
            if identifier == .rootContainer { return root }
            precondition(identifier.rawValue == "batch-root")
            if phase < 2 { return late }
            throw NSFileProviderError(.noSuchItem)
        }
        try await retirePlaceholder(batch: "batch", ledger: ledger(nil), manager: pending, pause: {
            switch phase {
            case 0: try FileManager.default.createDirectory(at: late, withIntermediateDirectories: false)
            case 1: break // provider identity is gone; filesystem deletion still pending
            case 2: try FileManager.default.removeItem(at: late)
            default: preconditionFailure("retirement did not finish after deletion")
            }
            phase += 1
        })
        precondition(phase == 3 && pending.signals == 1 && pending.lookups == 4)

        for failure in [NSFileProviderError(.serverUnreachable) as Error, CocoaError(.fileReadNoSuchFile)] {
            let failed = RetirementDomain()
            failed.resolve = { _ in throw failure }
            do {
                try await retirePlaceholder(batch: "batch", ledger: ledger(parent), manager: failed)
                preconditionFailure("lookup failure acknowledged as retirement")
            } catch { precondition((error as NSError).domain == (failure as NSError).domain && (error as NSError).code == (failure as NSError).code) }
        }
        let denied = RetirementDomain()
        do {
            try await retirePlaceholder(batch: "batch", ledger: ledger(parent), manager: denied,
                                        inspect: { _ in throw ItemRejection.accessDenied })
            preconditionFailure("namespace access failure acknowledged as retirement")
        } catch { precondition(error as? ItemRejection == .accessDenied) }
        let unsignaled = RetirementDomain()
        unsignaled.signalFailure = NSFileProviderError(.serverUnreachable)
        do {
            try await retirePlaceholder(batch: "batch", ledger: ledger(parent), manager: unsignaled)
            preconditionFailure("failed signal acknowledged as retirement")
        } catch { precondition(unsignaled.lookups == 0) }
        let cancelled = RetirementDomain()
        cancelled.resolve = { _ in parent }
        do {
            try await retirePlaceholder(batch: "batch", ledger: ledger(parent), manager: cancelled,
                                        pause: { throw CancellationError() })
            preconditionFailure("pending identity acknowledged before cancellation")
        } catch { precondition(error is CancellationError && cancelled.signals == 1) }
        print("Placeholder retirement: unrelated domain work, delayed creation/rename, strict removal, retries and failure gates passed")
    }
    static func failureDiagnosticsKeepCodesWithoutPrivateData() throws {
        let underlying = NSError(domain: NSPOSIXErrorDomain, code: 13,
            userInfo: [NSLocalizedDescriptionKey: "/private/secret-file token=secret"])
        let error = NSError(domain: NSCocoaErrorDomain, code: 4097,
            userInfo: [NSUnderlyingErrorKey: underlying, NSFilePathErrorKey: "/private/secret-file"])
        let diagnostic = providerFailureDiagnostic(error)
        precondition(diagnostic.contains("4097") && diagnostic.contains("13"))
        precondition(diagnostic.contains(NSCocoaErrorDomain) && diagnostic.contains(NSPOSIXErrorDomain))
        precondition(!diagnostic.contains("private") && !diagnostic.contains("secret"))
        let unknown = providerFailureDiagnostic(NSError(domain: "/private/secret-domain", code: 9))
        precondition(unknown.contains("other") && !unknown.contains("secret"))
    }
    static func publicationIncludesRootSymlinks() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let folder = root.appendingPathComponent("batch")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        for name in ["first.txt", "other.txt"] { try Data("payload".utf8).write(to: folder.appendingPathComponent(name)) }
        let linkURL = folder.appendingPathComponent("alias")
        try FileManager.default.createSymbolicLink(atPath: linkURL.path, withDestinationPath: "first.txt")
        let parent = Record(id: "batch-root", parent: ".root", name: "batch", directory: true, size: nil, modified: nil, node: nil)
        let link = Record(id: "batch-1", parent: parent.id, name: "alias", directory: false, size: 9, modified: nil, node: 1, version: 1, symlink_target: "first.txt")
        let files = ["first.txt", "other.txt"].enumerated().map { index, name in
            Record(id: "batch-\(index + 2)", parent: parent.id, name: name, directory: false, size: 7, modified: nil, node: UInt64(index + 2))
        }
        precondition(finalItemMatches(link, url: linkURL, metadata: visibleMetadata(linkURL)), "ready root symlink rejected")
        let records = [link] + files
        let urls = try await resolvePublishedItemURLs(parent: parent, children: records, resolve: { record in
            record.id == parent.id ? folder : folder.appendingPathComponent(record.name)
        }, pause: { throw CocoaError(.userCancelled) })
        precondition(urls == records.map { folder.appendingPathComponent($0.name).path })
        let regular = Record(id: link.id, parent: parent.id, name: link.name, directory: false, size: 9, modified: nil, node: 1)
        precondition(!finalItemMatches(regular, url: linkURL, metadata: visibleMetadata(linkURL)), "link accepted as a regular placeholder")
        try FileManager.default.removeItem(at: linkURL)
        try Data(repeating: 1, count: 9).write(to: linkURL)
        precondition(!finalItemMatches(link, url: linkURL, metadata: visibleMetadata(linkURL)), "regular placeholder accepted as link")
        try FileManager.default.removeItem(at: linkURL)
        try FileManager.default.createSymbolicLink(atPath: linkURL.path, withDestinationPath: "other.txt")
        precondition(!finalItemMatches(link, url: linkURL, metadata: visibleMetadata(linkURL)), "same-size wrong link accepted")
        try FileManager.default.removeItem(at: linkURL)
        try FileManager.default.createSymbolicLink(atPath: linkURL.path, withDestinationPath: "first.txt")
        try FileManager.default.removeItem(at: folder.appendingPathComponent("first.txt"))
        precondition(finalItemMatches(link, url: linkURL, metadata: visibleMetadata(linkURL)), "publication followed the link target")
    }

    static func publicationRejectsInvalidFinalItemsImmediately() async throws {
        let file = Record(id: "batch-1", parent: "batch-root", name: "file", directory: false, size: 9, modified: nil, node: 1)
        let link = Record(id: file.id, parent: file.parent, name: file.name, directory: false, size: 9, modified: nil, node: 1, symlink_target: "first.txt")
        let url = URL(fileURLWithPath: "/fixture/file")
        var pauses = 0
        for (record, metadata, expected) in [
            (file, nil, ItemRejection.missing),
            (file, VisibleMetadata(directory: true, size: 9, hidden: false), .wrongKind),
            (link, VisibleMetadata(directory: false, size: 9, hidden: false), .wrongKind),
            (link, VisibleMetadata(directory: false, size: 9, hidden: false, symlinkTarget: "other.txt"), .wrongTarget)
        ] {
            do {
                _ = try await resolveFinalItemURL(record, resolve: { url }, inspect: { _ in metadata }, pause: { pauses += 1; throw CocoaError(.userCancelled) })
                preconditionFailure("rejected item became ready")
            } catch { precondition(error as? ItemRejection == expected) }
        }
        do {
            _ = try await resolveFinalItemURL(file, resolve: { throw NSError(domain: NSFileProviderErrorDomain, code: NSFileProviderError.noSuchItem.rawValue) }, pause: { pauses += 1 })
            preconditionFailure("provider's missing item retried")
        } catch { precondition(error as? ItemRejection == .missing) }
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let fifo = root.appendingPathComponent("file")
        precondition(mkfifo(fifo.path, mode_t(0o600)) == 0)
        do {
            _ = try await resolveFinalItemURL(file, resolve: { fifo }, pause: { pauses += 1 })
            preconditionFailure("unsupported item retried")
        } catch { precondition(error as? ItemRejection == .unsupportedType) }
        do {
            try await waitForItemRemoval(fifo, pause: { pauses += 1 })
            preconditionFailure("unsupported item treated as retired")
        } catch { precondition(error as? ItemRejection == .unsupportedType) }
        do {
            try await waitForItemRemoval(url, inspect: { _ in throw ItemRejection.accessDenied }, pause: { pauses += 1 })
            preconditionFailure("denied retirement retried")
        } catch { precondition(error as? ItemRejection == .accessDenied) }
        let parent = Record(id: "batch-root", parent: ".root", name: "fixture", directory: true, size: nil, modified: nil, node: nil)
        var rejected: ItemRejection?
        do {
            _ = try await resolvePublishedItemURLs(parent: parent, children: [file], resolve: { record in
                record.id == parent.id ? url.deletingLastPathComponent() : url
            }, inspect: { _ in VisibleMetadata(directory: true, size: 0, hidden: false) }, rejected: { rejected = $0 }, pause: { pauses += 1 })
            preconditionFailure("published item rejection was swallowed")
        } catch { precondition(error as? ItemRejection == .wrongKind) }
        precondition(rejected == .wrongKind, "manager must receive the rejection before its task group unwinds")
        precondition(pauses == 0, "a definitive rejection entered the retry/deadline path")
        // A cached generic name really is pending, and still honours cancellation.
        do {
            _ = try await resolveFinalItemURL(file, resolve: { url.appendingPathExtension("generic") }, inspect: { _ in nil }, pause: { pauses += 1; throw CocoaError(.userCancelled) })
            preconditionFailure("pending callback ignored cancellation")
        } catch { precondition((error as NSError).code == CocoaError.userCancelled.rawValue) }
        precondition(pauses == 1)
    }

    static func main() async throws {
        try await placeholderRetirementIsItemScoped()
        try failureDiagnosticsKeepCodesWithoutPrivateData()
        try await publicationIncludesRootSymlinks()
        try await publicationRejectsInvalidFinalItemsImmediately()

        try await testLaunchRecovery()
        try await testDomainRemoval()
        precondition(ProviderUpdateAction(rawValue: "publish") == nil)
        precondition(ProviderUpdateAction(rawValue: "prepare-update") == .prepare)
        precondition(ProviderUpdateAction(rawValue: "finish-update") == .finish)
        var updateCalls: [String] = []
        try await ProviderUpdateAction.prepare.perform(disconnect: { options in
            precondition(options == [.temporary])
            updateCalls.append("disconnect")
        }, reconnect: { updateCalls.append("reconnect") })
        precondition(updateCalls == ["disconnect"], "update preparation reconnected before replacement")
        updateCalls.append("replace")
        try await ProviderUpdateAction.finish.perform(disconnect: { _ in
            preconditionFailure("finishing an update disconnected the new extension")
        }, reconnect: { updateCalls.append("reconnect") })
        precondition(updateCalls == ["disconnect", "replace", "reconnect"])
        for action in [ProviderUpdateAction.prepare, .finish] {
            do {
                try await action.perform(disconnect: { _ in throw CocoaError(.fileWriteNoPermission) },
                                         reconnect: { throw CocoaError(.fileWriteNoPermission) })
                preconditionFailure("failed provider update lifecycle acknowledged")
            } catch { precondition((error as NSError).code == CocoaError.fileWriteNoPermission.rawValue) }
        }
        for invalid in [
            #"{"size":1,"version":1}"#,
            #"{"schema":2,"size":1,"version":1}"#,
            #"{"schema":1,"size":1,"version":1,"action":"unknown"}"#
        ] {
            do {
                _ = try JSONDecoder().decode(CurrentMetadata.self, from: Data(invalid.utf8))
                preconditionFailure("invalid local metadata accepted")
            } catch {}
        }
        for invalid in [
            #"{"token":"secret","port":1,"records":[],"urls":[]}"#,
            #"{"schema":2,"token":"secret","port":1,"records":[],"urls":[]}"#,
            #"{"schema":1,"token":"secret","port":1,"records":[],"urls":[],"execute":true}"#
        ] {
            do {
                _ = try JSONDecoder().decode(Catalog.self, from: Data(invalid.utf8))
                preconditionFailure("invalid local catalog accepted")
            } catch {}
        }
        // The extension reads the catalog view. Durable source credentials
        // belong to the Rust backend and must not become native item metadata.
        _ = try JSONDecoder().decode(Catalog.self, from: Data(#"{"schema":1,"token":"bridge","port":1,"records":[],"urls":[],"source":{"provisional":false,"token":[]}}"#.utf8))
        _ = try JSONDecoder().decode(PlaceholderLedger.self, from: Data(#"{"schema":1,"parent":null,"urls":[]}"#.utf8))
        let finalFile = Record(id: "batch-slot-file-0", parent: "batch-root", name: "actual.png", directory: false,
                               size: 12345, modified: nil, node: 7)
        let expectedURL = URL(fileURLWithPath: "/private-parent/actual.png")
        precondition(!finalItemMatches(finalFile, url: URL(fileURLWithPath: "/private-parent/generic.data"),
                                       metadata: VisibleMetadata(directory: false, size: 0, hidden: false)), "old quick URL escaped")
        precondition(!finalItemMatches(finalFile, url: expectedURL,
                                       metadata: VisibleMetadata(directory: false, size: 0, hidden: false)), "stale size escaped")
        precondition(!finalItemMatches(finalFile, url: expectedURL,
                                       metadata: VisibleMetadata(directory: true, size: 12345, hidden: false)), "wrong kind escaped")
        precondition(!finalItemMatches(finalFile, url: expectedURL,
                                       metadata: VisibleMetadata(directory: false, size: 12345, hidden: true)), "prepared hidden flag escaped")
        precondition(!finalItemMatches(finalFile, url: expectedURL, metadata: nil))
        precondition(finalItemMatches(finalFile, url: expectedURL,
                                      metadata: VisibleMetadata(directory: false, size: 12345, hidden: false)))
        var resolutions = 0
        let confirmed = try await resolveFinalItemURL(finalFile, resolve: {
            resolutions += 1
            return resolutions == 1 ? URL(fileURLWithPath: "/private-parent/generic.data") : expectedURL
        }, inspect: { _ in VisibleMetadata(directory: false, size: resolutions < 3 ? 0 : 12345, hidden: false) }, pause: {})
        precondition(confirmed == expectedURL && resolutions == 3)
        // The child's final basename and metadata can precede its parent's
        // rename. A parallel lookup used to hand out that now-stale child path.
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let privateParent = directory.appendingPathComponent("private-parent")
        let finalParent = directory.appendingPathComponent("final-parent")
        try FileManager.default.createDirectory(at: privateParent, withIntermediateDirectories: true)
        try Data(repeating: 1, count: 12345).write(to: privateParent.appendingPathComponent(finalFile.name))
        let parentRecord = Record(id: finalFile.parent, parent: ".root", name: "final-parent", directory: true,
                                  size: nil, modified: nil, node: nil, version: 1)
        var parentResolved = false
        let published = try await resolvePublishedItemURLs(parent: parentRecord, children: [finalFile], resolve: { record in
            if record.directory {
                try FileManager.default.moveItem(at: privateParent, to: finalParent)
                parentResolved = true
                return finalParent
            }
            precondition(parentResolved, "child resolution raced the parent rename")
            return finalParent.appendingPathComponent(record.name)
        })
        precondition(published == [finalParent.appendingPathComponent(finalFile.name).path])
        precondition(FileManager.default.fileExists(atPath: published[0]))
        // Matching basename/size in a different still-existing parent must not
        // satisfy readiness either (e.g. a cached URL for a retired spare).
        try FileManager.default.createDirectory(at: privateParent, withIntermediateDirectories: true)
        try Data(repeating: 1, count: 12345).write(to: privateParent.appendingPathComponent(finalFile.name))
        var childResolutions = 0
        let inFinalParent = try await resolveFinalItemURL(finalFile, parent: finalParent, resolve: {
            childResolutions += 1
            return (childResolutions == 1 ? privateParent : finalParent).appendingPathComponent(finalFile.name)
        }, pause: {})
        precondition(childResolutions == 2 && inFinalParent.path == published[0])
        do {
            _ = try await resolveFinalItemURL(finalFile, resolve: { expectedURL.appendingPathExtension("generic") }, inspect: { _ in nil },
                                              pause: { throw CocoaError(.userCancelled) })
            preconditionFailure("cancelled rename became ready")
        } catch { precondition((error as NSError).code == CocoaError.userCancelled.rawValue) }
        let generic = Record(id: "batch-slot-file-0", parent: "batch-root", name: ".Oink-slot-file-0", directory: false,
                             size: nil, modified: nil, node: UInt64.max, version: 1, hidden: true)
        let folderBefore = Record(id: "batch-slot-folder-0", parent: "batch-root", name: ".Oink-slot-folder-0", directory: true,
                                  size: nil, modified: nil, node: 99, version: 1, hidden: true)
        let folderAfter = Record(id: folderBefore.id, parent: folderBefore.parent, name: "actual folder", directory: true,
                                 size: nil, modified: nil, node: 7, version: 1)
        precondition(Item(generic).fileSystemFlags.contains(.hidden))
        precondition(!Item(finalFile).fileSystemFlags.contains(.hidden))
        precondition(Item(folderBefore).itemVersion.metadataVersion != Item(folderAfter).itemVersion.metadataVersion)
        for placeholder in [generic, folderBefore] {
            do {
                _ = try bridgeRequest(placeholder.directory ? "list" : "read", catalog: Catalog(token: "old", port: 1, records: [placeholder]), record: placeholder)
                preconditionFailure("prepared child obtained content capability")
            } catch { precondition((error as NSError).code == NSFileProviderError.noSuchItem.rawValue) }
        }
        var preparationSteps: [String] = []
        try await prepareSpareDirectory(resolve: {
            preparationSteps.append("visible"); return URL(fileURLWithPath: "/fixture")
        }, download: { preparationSteps.append("download-completed") }, materialized: { _ in
            preparationSteps.append("materialized"); return true
        })
        precondition(preparationSteps == ["visible", "download-completed", "materialized"])
        for failure in ["resolve", "download", "dataless"] {
            var checked = false
            do {
                try await prepareSpareDirectory(resolve: {
                    if failure == "resolve" { throw CocoaError(.fileReadUnknown) }
                    return URL(fileURLWithPath: "/fixture")
                }, download: {
                    if failure == "download" { throw CocoaError(.fileReadUnknown) }
                }, materialized: { _ in checked = true; return false })
                preconditionFailure("unacknowledged directory marked ready")
            } catch { precondition(checked == (failure == "dataless")) }
        }
        let download = downloadProgress()
        precondition(download.isIndeterminate && download.kind == .file)
        precondition(download.fileOperationKind == .downloading && download.fileTotalCount == 1)
        download.totalUnitCount = 4 * 1024 * 1024
        download.completedUnitCount = 1024 * 1024
        precondition(download.fractionCompleted == 0.25 && download.fileCompletedCount == 0)
        precondition(!download.localizedDescription.isEmpty && !download.localizedAdditionalDescription.isEmpty)
        let current = try JSONDecoder().decode(CurrentMetadata.self, from: Data(#"{"schema":1,"size":209715200,"modified":1700000001234,"version":1}"#.utf8))
        func item(_ size: UInt64, _ modified: Int64?) -> Item {
            Item(Record(id: "batch-0", parent: "batch", name: "中文.bin", directory: false, size: size, modified: modified, node: 0))
        }
        let offeredRecord = Record(id: "stable", parent: "parent", name: "file", directory: false,
                                   size: 3, modified: 1, node: 7, version: 10, hidden: false)
        let latestRecord = offeredRecord.updating(current)
        precondition(latestRecord.id == offeredRecord.id && latestRecord.node == offeredRecord.node)
        precondition(latestRecord.parent == offeredRecord.parent && latestRecord.name == offeredRecord.name)
        precondition(latestRecord.size == current.size && latestRecord.modified == current.modified)
        precondition(latestRecord.version == current.version && latestRecord.hidden == offeredRecord.hidden)
        precondition(Item(offeredRecord).itemVersion != Item(latestRecord).itemVersion)
        let copied = item(1048576, 1700000000000)
        let fetched = item(current.size, current.modified)
        // Exercise the optional Objective-C property the system reads, for both enumeration
        // and the fresh Item returned by fetchContents. Both capabilities and filesystem
        // flags must describe an ordinary copy; POSIX write permission alone left uchg.
        for record in [copied.record, fetched.record, rootItem().record,
                       Record(id: "directory", parent: "batch", name: "directory", directory: true, size: 0, modified: nil, node: 1)] {
            let exported: NSFileProviderItem = Item(record)
            let flags: NSFileProviderFileSystemFlags = record.directory
                ? [.userReadable, .userWritable, .userExecutable] : [.userReadable, .userWritable]
            precondition(exported.fileSystemFlags == flags)
            var expected:NSFileProviderItemCapabilities=[.allowsReading,.allowsWriting,.allowsTrashing,.allowsDeleting]
            if record.node != nil {expected.formUnion([.allowsRenaming,.allowsReparenting])}
            precondition(exported.capabilities == expected)
            if record.directory {
                precondition(exported.capabilities!.contains([.allowsContentEnumerating, .allowsAddingSubItems]))
            }
            let size = record.size.map(String.init) ?? "unknown"
            let modified = record.modified.map(String.init) ?? "unknown"
            precondition(exported.itemVersion!.contentVersion == Data("\(record.version)-\(size)-\(modified)".utf8))

        }
        let pendingDirectory = Item(Record(id: "pending", parent: "batch", name: "pending", directory: true, size: nil, modified: nil, node: 8))
        precondition(pendingDirectory.documentSize == nil && pendingDirectory.childItemCount == nil)
        precondition(fetched.documentSize?.uint64Value == 209715200)
        precondition(fetched.contentModificationDate == Date(timeIntervalSince1970: 1700000001.234))
        precondition(copied.itemVersion.contentVersion != fetched.itemVersion.contentVersion)
        precondition(item(current.size, 1700000000000).itemVersion.contentVersion != fetched.itemVersion.contentVersion)
        precondition(item(0, nil).itemVersion.contentVersion != item(0, 0).itemVersion.contentVersion)
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        func save(_ records: [Record]) throws {
            try JSONEncoder().encode(Catalog(token: "capability", port: 1234, records: records))
                .write(to: root.appendingPathComponent("batch.json"), options: .atomic)
        }
        // Real allocated-byte accounting and advisory writer protection in an isolated root.
        let staging = root.appendingPathComponent("temporary")
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        let active = staging.appendingPathComponent("active")
        let idle = staging.appendingPathComponent("idle")
        let handoff = staging.appendingPathComponent("handoff-item-0-copy")
        for url in [active,idle,handoff] { try Data(repeating: 7,count: 9000).write(to: url) }
        let writer = try FileHandle(forUpdating: active); defer { try? writer.close() }
        precondition(flock(writer.fileDescriptor, LOCK_EX) == 0)
        let before = try inspectTemporary(in: staging,clear:false,evicted:[])
        let cleared = try assertCacheMutation { try inspectTemporary(in: staging,clear:true,evicted:[]) }
        precondition(cleared.removed > 0 && before.bytes == cleared.bytes + cleared.removed)
        precondition(cleared.bytes == cleared.kept)
        precondition(FileManager.default.fileExists(atPath: active.path))
        precondition(FileManager.default.fileExists(atPath: handoff.path))
        precondition(!FileManager.default.fileExists(atPath: idle.path))
        // Exercise APFS clone independence directly; this is filesystem evidence,
        // not a substitute for a signed File Provider eviction test.
        let outside = root.appendingPathComponent("user-copy")
        precondition(clonefile(handoff.path,outside.path,0) == 0)
        let second = try assertCacheMutation { try inspectTemporary(in:staging,clear:true,evicted:["item-0"]) }
        precondition(second.removed > 0 && !FileManager.default.fileExists(atPath: handoff.path))
        let copiedData = try Data(contentsOf: outside)
        precondition(copiedData == Data(repeating:7,count:9000))
        let poolKey = String(repeating: "a", count: 64)
        let finalSlot = Record(id: poolKey + "-slot-file-0", parent: poolKey + "-root", name: "final.txt", directory: false,
                               size: 4, modified: nil, node: 1, version: 1)
        let oldSlot = Record(id: finalSlot.id, parent: finalSlot.parent, name: ".Oink-slot-file-0", directory: false,
                             size: nil, modified: nil, node: UInt64.max, version: 1, hidden: true)
        let poolCatalog = root.appendingPathComponent(poolKey + ".json")
        let poolMarker = root.appendingPathComponent(poolKey + ".enumerated")
        try JSONEncoder().encode(Catalog(token: "secret", port: 1, records: [finalSlot])).write(to: poolCatalog)
        recordChildrenEnumerated([oldSlot], in: root)
        precondition(!FileManager.default.fileExists(atPath: poolMarker.path), "old generic enumeration counted as final children")
        recordChildrenEnumerated([finalSlot], in: root)
        precondition(FileManager.default.fileExists(atPath: poolMarker.path))
        try FileManager.default.removeItem(at: poolCatalog)
        try FileManager.default.removeItem(at: poolMarker)
        let empty = try CatalogSnapshot.capture(in: root)
        precondition(empty.anchor.rawValue.count <= 500)
        for _ in 0..<100 {
            let repeated = try CatalogSnapshot.capture(in: root)
            precondition(repeated.anchor == empty.anchor)
            let delta = repeated.changes(since: empty, in: .workingSet)
            precondition(delta.updated.isEmpty && delta.deleted.isEmpty)
        }
        let folder = Record(id: "batch", parent: NSFileProviderItemIdentifier.rootContainer.rawValue, name: "batch", directory: true, size: 0, modified: nil, node: nil)
        let file = copied.record
        let nested = Record(id: "aaa", parent: "batch", name: "nested", directory: true, size: 0, modified: nil, node: 1)
        let deep = Record(id: "000", parent: "aaa", name: "deep.txt", directory: false, size: 1, modified: nil, node: 2)
        try save([deep, file, nested, folder])
        let tree = try CatalogSnapshot.capture(in: root)
        let order = tree.scoped(to: .workingSet).map { $0.id }
        precondition(order.first == "batch" && order.firstIndex(of: "aaa")! < order.firstIndex(of: "000")!)
        let urls = try await resolveItemURLs([file, nested, deep]) { record in
            try await Task.sleep(nanoseconds: record.id == "batch-0" ? 20_000_000 : 1_000_000)
            return "/" + record.id
        }
        precondition(urls == ["/batch-0", "/aaa", "/000"])

        try save([file, folder])
        let added = try CatalogSnapshot.capture(in: root)
        precondition(added.anchor != empty.anchor)
        precondition(added.changes(since: empty, in: .workingSet).updated.count == 2)
        precondition(added.changes(since: empty, in: .rootContainer).updated == [folder])
        precondition(added.scoped(to: .trashContainer).isEmpty)
        precondition(added.scoped(to: NSFileProviderItemIdentifier(file.id)) == [file])
        // File order and process lifetime must not change the anchor.
        try save([folder, file])
        let reordered = try CatalogSnapshot.capture(in: root)
        precondition(reordered.anchor == added.anchor)
        let persisted = try CatalogSnapshot.load(added.anchor, in: root)
        precondition(persisted.records == added.records)
        // A corrupt cache entry under the current content hash must never be
        // handed out as a fresh anchor during an expired-anchor restart.
        let currentFile = root.appendingPathComponent("anchors").appendingPathComponent(String(data: added.anchor.rawValue, encoding: .utf8)!)
        try Data("[{".utf8).write(to: currentFile)
        let repaired = try CatalogSnapshot.capture(in: root)
        precondition(repaired.anchor == added.anchor)
        let repairedSnapshot = try CatalogSnapshot.load(repaired.anchor, in: root)
        precondition(repairedSnapshot.records == added.records)
        // The invalid-schema anchor exists on disk and contains identical records. It must still
        // expire so the OS requests a fresh enumeration with the new export properties.
        let invalidSchemaKey = String(data: added.anchor.rawValue, encoding: .utf8)!.replacingOccurrences(of: "oink-v3-", with: "wrong-v3-")
        try JSONEncoder().encode(added.records).write(to: root.appendingPathComponent("anchors").appendingPathComponent(invalidSchemaKey))
        do {
            _ = try CatalogSnapshot.load(NSFileProviderSyncAnchor(Data(invalidSchemaKey.utf8)), in: root)
            preconditionFailure("pre-export-policy anchor accepted")
        } catch { precondition((error as NSError).code == NSFileProviderError.syncAnchorExpired.rawValue) }
        try save([folder, fetched.record])
        let modified = try CatalogSnapshot.capture(in: root)
        precondition(modified.changes(since: persisted, in: .workingSet).updated == [fetched.record])
        try FileManager.default.removeItem(at: root.appendingPathComponent("batch.json"))
        let removed = try CatalogSnapshot.capture(in: root)
        let deletion = removed.changes(since: persisted, in: .workingSet)
        precondition(deletion.updated.isEmpty && Set(deletion.deleted.map { $0.rawValue }) == ["batch", "batch-0"])
        precondition(removed.anchor == empty.anchor)
        for data in [Data("oink-v1".utf8), Data("../../outside".utf8), Data(("oink-v3-" + String(repeating: "0", count: 64)).utf8)] {
            do {
                _ = try CatalogSnapshot.load(NSFileProviderSyncAnchor(data), in: root)
                preconditionFailure("unknown anchor accepted")
            } catch { precondition((error as NSError).code == NSFileProviderError.syncAnchorExpired.rawValue) }
        }
        print("File Provider ordinary export flags/capabilities, metadata, stable anchors, upgrade/restart, additions, changes, deletions and invalid-anchor tests passed")
    }

    static func testLaunchRecovery() async throws {
        for enabled in [false, true] {
            for disconnected in [false, true] {
                var calls: [String] = []
                try await recoverProviderLaunch(hidden: true, enabled: enabled, disconnected: disconnected,
                    disconnect: { options in
                        precondition(options == [.temporary]); calls.append("disconnect")
                    }, reconnect: { calls.append("reconnect") },
                    finishUpgrade: { calls.append("upgrade") },
                    resolveRoot: { calls.append("namespace-ready"); return URL(fileURLWithPath: "/Oink") })
                precondition(calls == (!enabled ? [] : disconnected ? ["reconnect", "upgrade", "namespace-ready"] : ["disconnect", "reconnect", "upgrade", "namespace-ready"]),
                             "reconnect acknowledgement must not bypass namespace readiness")
            }
        }
        do {
            try await recoverProviderLaunch(hidden: true, enabled: true, disconnected: false,
                disconnect: { _ in throw CocoaError(.fileWriteNoPermission) },
                reconnect: { preconditionFailure("reconnected after failed disconnect") },
                finishUpgrade: { preconditionFailure("upgrade after failed disconnect") },
                resolveRoot: { preconditionFailure("root after failed disconnect") })
            preconditionFailure("recovery failure acknowledged")
        } catch {}
        var ready = false
        do {
            try await recoverProviderLaunch(hidden: true, enabled: true, disconnected: true,
                disconnect: { _ in preconditionFailure("already suspended") }, reconnect: {},
                finishUpgrade: {}, resolveRoot: { throw CocoaError(.fileReadUnknown) })
            ready = true
        } catch {}
        precondition(!ready, "a wedged namespace must leave launch recovery pending")
        do {
            try await recoverProviderLaunch(hidden: true, enabled: true, disconnected: false,
                disconnect: { _ in }, reconnect: { throw CocoaError(.fileWriteNoPermission) },
                finishUpgrade: { preconditionFailure("upgrade after failed reconnect") },
                resolveRoot: { preconditionFailure("root after failed reconnect") })
            preconditionFailure("failed reconnect acknowledged")
        } catch {}
        // Model the reported relaunch: retained work needs a new bridge, which
        // the backend can start only after Recover returns. Native namespace
        // readiness must not wait for that work (or its remote source) to finish.
        final class RetainedDaemon {
            var bridge = false
            var pending = 109
            var namespaceQueries = 0
            func root() throws -> URL {
                precondition(!bridge && pending == 109)
                namespaceQueries += 1
                return URL(fileURLWithPath: "/Oink")
            }
            func download() throws {
                guard bridge else { throw NSFileProviderError(.serverUnreachable) }
                pending -= 1
            }
        }
        let daemon = RetainedDaemon()
        do { try daemon.download(); preconditionFailure("old bridge was usable") } catch {}
        try await recoverProviderLaunch(hidden: true, enabled: true, disconnected: true,
            disconnect: { _ in preconditionFailure("already suspended") }, reconnect: {},
            finishUpgrade: {}, resolveRoot: daemon.root)
        precondition(daemon.namespaceQueries == 1 && daemon.pending == 109)
        daemon.bridge = true
        try daemon.download()
        precondition(daemon.pending == 108, "recovery prevented content service from starting")
        for flags in [(false, true, false), (true, false, false), (true, true, true)] {
            do {
                try await verifyProviderLaunch(hidden: flags.0, enabled: flags.1, disconnected: flags.2,
                    resolveRoot: { preconditionFailure("unusable domain queried root") })
                preconditionFailure("unusable domain passed readiness recheck")
            } catch {}
        }
        // A local upgrade failure cannot be mistaken for mere readiness delay.
        do {
            try await recoverProviderLaunch(hidden: true, enabled: true, disconnected: true,
                disconnect: { _ in }, reconnect: {},
                finishUpgrade: { throw CocoaError(.fileWriteNoPermission) },
                resolveRoot: { preconditionFailure("unfinished upgrade released startup") })
            preconditionFailure("failed upgrade acknowledged")
        } catch {}
        print("File Provider launch recovery: ordering, root readiness, retained downloads without a bridge, inactive domains and failure gates passed")
    }

    static func testDomainRemoval() async throws {
        precondition(ProviderRemovalAction(rawValue: "remove-domain") == .remove)
        for other in ["publish", "prepare-update", "finish-update", "remove", ""] {
            precondition(ProviderRemovalAction(rawValue: other) == nil)
        }
        let current = NSFileProviderDomain(identifier: .init(providerDomain), displayName: "Oink")
        precondition(providerDomain == "oink-files", "upgrade must preserve the existing domain identity")
        let previousID = providerDomain + "-links"
        let previous = NSFileProviderDomain(identifier: .init(previousID), displayName: "Oink")
        let unrelated = NSFileProviderDomain(identifier: .init("unrelated-domain"), displayName: "Oink")
        let registered = [previous, unrelated, current]
        let removal = ProviderRemovalAction.remove
        precondition(removal.domains(registered, uninstall: false).map { $0.identifier.rawValue } == [providerDomain],
                     "ordinary operations must select the original single domain")
        let uninstallDomains = removal.domains(registered, uninstall: true)
        precondition(uninstallDomains.map { $0.identifier.rawValue } == [previousID, providerDomain],
                     "uninstall must tolerate the abandoned build 48 domain")
        var uninstallCalls: [String] = []
        let uninstallResult = try await removal.perform(domains: uninstallDomains, remove: { domain, mode in
            precondition(mode == .removeAll)
            uninstallCalls.append(domain.identifier.rawValue)
            return nil
        }, remaining: { [] })
        precondition(uninstallResult == "removed" && uninstallCalls == [previousID, providerDomain])
        // A surviving previous domain must fail verification too.
        do {
            _ = try await removal.perform(domains: uninstallDomains, remove: { _, _ in nil },
                remaining: { removal.domains([previous, unrelated], uninstall: true) })
            preconditionFailure("previous registered domain acknowledged as removed")
        } catch ProviderRemovalError.domainsRemain {}
        let absent = try await ProviderRemovalAction.remove.perform(domains: [], remove: { _, _ in
            preconditionFailure("absent domain was removed")
        }, remaining: { preconditionFailure("absent removal performed more work") })
        precondition(absent == "absent")

        let hidden = DisconnectedRemovalDomain(identifier: .init(providerDomain), displayName: "Oink")
        hidden.isHidden = true
        let visible = NSFileProviderDomain(identifier: .init("additional-domain"), displayName: "Oink")
        let domains = [hidden, visible]
        var calls: [String] = []
        let result = try await ProviderRemovalAction.remove.perform(domains: domains, remove: { domain, mode in
            precondition(mode == .removeAll, "uninstall preserved managed files")
            await Task.yield()
            calls.append(domain.identifier.rawValue)
            return nil
        }, remaining: {
            precondition(calls == domains.map { $0.identifier.rawValue }, "removal was not awaited for every domain")
            calls.append("verified")
            return []
        })
        precondition(result == "removed" && calls.last == "verified")

        calls = []
        do {
            _ = try await ProviderRemovalAction.remove.perform(domains: domains, remove: { domain, _ in
                calls.append(domain.identifier.rawValue)
                if domain.identifier == hidden.identifier { throw CocoaError(.fileWriteNoPermission) }
                return nil
            }, remaining: { [] })
            preconditionFailure("failed removal acknowledged")
        } catch { precondition((error as NSError).code == CocoaError.fileWriteNoPermission.rawValue) }
        precondition(calls == domains.map { $0.identifier.rawValue }, "one failed domain prevented removal of others")

        do {
            _ = try await ProviderRemovalAction.remove.perform(domains: domains, remove: { _, _ in nil },
                                                               remaining: { [hidden] })
            preconditionFailure("registered domain acknowledged as removed")
        } catch ProviderRemovalError.domainsRemain {}
        do {
            _ = try await ProviderRemovalAction.remove.perform(domains: [hidden], remove: { _, _ in
                URL(fileURLWithPath: "/unexpected-preserved-data")
            }, remaining: { [] })
            preconditionFailure("preserved files acknowledged as wiped")
        } catch ProviderRemovalError.preservedData {}
        do {
            _ = try await ProviderRemovalAction.remove.perform(domains: domains, remove: { _, _ in nil },
                                                               remaining: { throw CocoaError(.fileReadUnknown) })
            preconditionFailure("failed domain verification acknowledged")
        } catch { precondition((error as NSError).code == CocoaError.fileReadUnknown.rawValue) }
        print("File Provider remove-domain parsing, absent, multiple/disconnected/hidden domains, removeAll, completion and failure tests passed")
    }
}
