#if !DEBUG
#error("Upgrade fixtures require a debug build")
#endif
import Foundation
import FileProvider
import Darwin

@main struct UpgradeTests {
    static func main() async throws {
        let fm = FileManager.default
        let fixture = fm.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try fm.createDirectory(at: fixture, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: fixture) }
        let source = fixture.appendingPathComponent("source")
        try Data("original".utf8).write(to: source)
        for stage in ["wiping", "cleared", "reset", "system-reset"] {
            let group = fixture.appendingPathComponent(stage)
            let upgrade = ProviderUpgrade(group: group)
            try fm.createDirectory(at: upgrade.root, withIntermediateDirectories: true)
            try Data("old invalid journal".utf8).write(to: upgrade.root.appendingPathComponent("old.received"))
            try Data("old invalid catalog".utf8).write(to: upgrade.root.appendingPathComponent("old.json"))
            try fm.createSymbolicLink(at: upgrade.root.appendingPathComponent("outside"), withDestinationURL: fixture)
            try fm.linkItem(at: source, to: upgrade.root.appendingPathComponent("source-link"))
            try Data("other app-group state".utf8).write(to: group.appendingPathComponent("keep"))
            var before = stat(), after = stat()
            precondition(lstat(upgrade.root.path, &before) == 0)
            let fault: (String) throws -> Void = { if $0 == stage { throw CocoaError(.userCancelled) } }
            do {
                try upgrade.prepareLocal(checkpoint: fault)
                try await upgrade.finish(registered: true, enabled: true, connected: true, reset: {}, checkpoint: fault)
                preconditionFailure("failure point did not interrupt upgrade")
            } catch { precondition((error as NSError).code == CocoaError.userCancelled.rawValue) }
            let pending = try upgrade.pending()
            precondition(pending)
            try upgrade.prepareLocal()
            let empty = try catalogs(in: upgrade.root)
            precondition(empty.isEmpty)
            do {
                try withReceived(in: upgrade.root) { preconditionFailure("pending upgrade admitted mutation") }
            } catch { precondition((error as NSError).code == NSFileProviderError.serverUnreachable.rawValue) }
            // No root/domain replacement is needed for the local transaction.
            precondition(lstat(upgrade.root.path, &after) == 0 && before.st_ino == after.st_ino)
            var resets = 0
            try await upgrade.finish(registered: true, enabled: true, connected: true, reset: {
                resets += 1
                let files = try fm.contentsOfDirectory(atPath: upgrade.root.path)
                precondition(files.isEmpty, "system reconciliation preceded local wipe")
            })
            try upgrade.requireReady()
            precondition(resets == 1)
            try Data("new".utf8).write(to: upgrade.root.appendingPathComponent("new.received"))
            try upgrade.prepareLocal()
            try await upgrade.finish(registered: true, enabled: true, connected: true, reset: { preconditionFailure("repeated completed wipe") })
            let new = try Data(contentsOf: upgrade.root.appendingPathComponent("new.received"))
            let original = try Data(contentsOf: source)
            precondition(new == Data("new".utf8) && original == Data("original".utf8))
            precondition(fm.fileExists(atPath: group.appendingPathComponent("keep").path))
        }
        let upgrade = ProviderUpgrade(group: fixture.appendingPathComponent("inactive"))
        try fm.createDirectory(at: upgrade.root, withIntermediateDirectories: true)
        for flags in [(false, true), (true, false)] {
            do {
                try await upgrade.finish(registered: true, enabled: flags.0, connected: flags.1,
                                         reset: { preconditionFailure("inactive domain reset") })
                preconditionFailure("inactive domain admitted publication")
            } catch { precondition((error as NSError).code == NSFileProviderError.serverUnreachable.rawValue) }
        }
        do {
            try await upgrade.finish(registered: true, enabled: true, connected: true,
                                     reset: { throw CocoaError(.fileReadUnknown) })
            preconditionFailure("failed system reset committed")
        } catch {}
        let pending = try upgrade.pending()
        precondition(pending)
        try await upgrade.finish(registered: false, enabled: false, connected: false,
                                 reset: { preconditionFailure("absent domain requested a native API") })
        // An explicit fresh-data reset also reuses the root, expires all old
        // anchors, and cannot parse or preserve the old received journal.
        let record = Record(id: "old-1", parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
                            name: "old", directory: false, size: 1, modified: nil, node: 1)
        try JSONEncoder().encode(Catalog(token: "", port: 0, records: [record], received_at: 1))
            .write(to: upgrade.root.appendingPathComponent("old.json"))
        let snapshot = try CatalogSnapshot.capture(in: upgrade.root)
        precondition(snapshot.records.map(\.id) == [record.id])
        let resetToken = String(repeating: "a", count: 32)
        try upgrade.prepareLocal(resetToken: resetToken)
        let emptySnapshot = try CatalogSnapshot.capture(in: upgrade.root)
        precondition(emptySnapshot.records.isEmpty)
        do {
            _ = try CatalogSnapshot.load(snapshot.anchor, in: upgrade.root)
            preconditionFailure("old anchor survived wipe")
        } catch { precondition((error as NSError).code == NSFileProviderError.syncAnchorExpired.rawValue) }
        try await upgrade.finish(registered: false, enabled: false, connected: false, reset: {})
        let newFile = upgrade.root.appendingPathComponent("new-local-file")
        try Data("new local content".utf8).write(to: newFile)
        // Native completion can precede the host's launch-marker commit. Replay
        // the same fresh-reset request without erasing files created in between.
        try upgrade.prepareLocal(resetToken: resetToken)
        precondition(fm.fileExists(atPath: newFile.path))
        try upgrade.prepareLocal(resetToken: String(repeating: "b", count: 32))
        precondition(!fm.fileExists(atPath: newFile.path))
        let redirected = ProviderUpgrade(group: fixture.appendingPathComponent("redirected"))
        try fm.createDirectory(at: redirected.group, withIntermediateDirectories: true)
        try fm.createSymbolicLink(at: redirected.root, withDestinationURL: fixture)
        do { try redirected.prepareLocal(); preconditionFailure("redirected root accepted") } catch {}
        let original = try Data(contentsOf: source)
        precondition(original == Data("original".utf8))
        precondition(providerDomain == "oink-files")
        print("Upgrade private fixtures: crash/resume at four phases, once-only wipe, inactive/failed reset, root identity, source links, old anchors and mutation gate passed")
    }
}
