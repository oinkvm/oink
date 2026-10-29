import Foundation
import FileProvider
import Darwin
@main struct ReceivedTests {
    static func main() throws {
        let root=FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at:root,withIntermediateDirectories:true)
        defer { try? FileManager.default.removeItem(at:root) }
        try maintenancePreservesReplacement(in: root.appendingPathComponent("replacement"))
        try consumerPreventsMaintenance(in: root.appendingPathComponent("consumer"))
        try emptyContainers(in: root.appendingPathComponent("empty"))
        try testReceivedHierarchyAndDates(in: root.appendingPathComponent("dates"))
        try directoryHandoffUsesReceivedVersion(in: root.appendingPathComponent("handoff-version"))
        try pinningSurvivesOldOverlay(in: root.appendingPathComponent("pinning"))
        try localEditRejectsLateSourceHandoff(in: root.appendingPathComponent("local-edit"))
        let record=Record(id:"batch-1",parent:"batch-root",name:"fixture",directory:false,size:4,modified:nil,node:1)
        let catalog=Catalog(token:"",port:0,records:[Record(id:"batch-root",parent:NSFileProviderItemIdentifier.rootContainer.rawValue,name:"batch",directory:true,size:nil,modified:nil,node:nil),record],received_at:1000)
        try JSONEncoder().encode(catalog).write(to:root.appendingPathComponent("batch.json"))
        try assertCacheMutation { try receivedHandoff(record,in:root,at:Date(timeIntervalSince1970:100)) }
        let stored=try readReceived("batch",in:root)[record.id]!
        precondition(stored.received_at==1000,"age starts at delivery, not first fetch")
        let protected=try receivedProtected("batch",in:root);precondition(protected)
        let original=try receivedOverlay(record,in:root);precondition(original == record)
        let renamed=Record(id:record.id,parent:record.parent,name:"renamed",directory:false,size:4,modified:nil,node:1)
        let changed=try assertCacheMutation { try receivedModify(Item(renamed),fields:[.filename],contents:nil,in:root) }
        precondition(changed.name=="renamed")
        precondition(Item(changed).itemVersion.metadataVersion != Item(record).itemVersion.metadataVersion)
        try assertCacheMutation { try receivedDelete([record.id],in:root) }
        let deleted=try receivedOverlay(record,in:root);precondition(deleted == nil)
        let retained=try receivedProtected("batch",in:root);precondition(!retained)
        // A stale Rust enumeration must never resurrect an explicit deletion.
        do { try receivedHandoff(record,in:root,at:Date());preconditionFailure("deleted item resurrected") } catch {}
        let stillDeleted=try receivedOverlay(record,in:root);precondition(stillDeleted == nil)
        // A directory read may publish a new child after the parent was deleted.
        try receivedDelete(["batch-root"],in:root)
        let late=Record(id:"batch-9",parent:"batch-root",name:"late",directory:false,size:4,modified:nil,node:9)
        try JSONEncoder().encode(Catalog(token:"",port:0,records:catalog.records+[late],received_at:1000)).write(to:root.appendingPathComponent("batch.json"))
        let visible=try catalogs(in:root).flatMap {$0.records}
        precondition(!visible.contains(where:{$0.id==late.id}),"late directory child revived a deleted subtree")
        do {try receivedHandoff(late,in:root);preconditionFailure("late child handoff revived a deleted parent")} catch {}
        try privatePreparationCannotOverridePublication(in: root)
        print("received deletion, handoff, restart and private preparation tests passed")
    }
    static func consumerPreventsMaintenance(in root: URL) throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let record = Record(id: "lease-1", parent: "lease-root", name: "received", directory: false,
                            size: 8, modified: nil, node: 1, version: 1)
        let url = root.appendingPathComponent(record.name)
        try Data("received".utf8).write(to: url)
        try JSONEncoder().encode(Catalog(token: "", port: 0, records: [record], received_at: 1))
            .write(to: root.appendingPathComponent("lease.json"))
        let fd = Darwin.open(root.appendingPathComponent("lease.consumers").path, O_CREAT | O_RDWR, 0o600)
        precondition(fd >= 0)
        defer { close(fd) }
        precondition(flock(fd, LOCK_SH | LOCK_NB) == 0)
        var identity = stat(); precondition(lstat(url.path, &identity) == 0)
        let removed = try removeReceivedNative(url, record: record, identity: identity, in: root)
        precondition(!removed && FileManager.default.fileExists(atPath: url.path),
                     "received maintenance deleted an unread native consumer's file")
        let active = try readReceived("lease", in: root)[record.id]
        precondition(active?.deleted != true)
        precondition(flock(fd, LOCK_UN) == 0)
        let retired = try assertCacheMutation { try removeReceivedNative(url, record: record, identity: identity, in: root) }
        precondition(retired && !FileManager.default.fileExists(atPath: url.path))
        let gone = try readReceived("lease", in: root)[record.id]
        precondition(gone?.deleted == true)
    }
    static func maintenancePreservesReplacement(in root: URL) throws {
        for directory in [false, true] {
            let root = root.appendingPathComponent(directory ? "directory" : "file")
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
            let url = root.appendingPathComponent("received")
            let saved = root.appendingPathComponent("moved-target")
            let record = Record(id: "replace-1", parent: "replace-root", name: "received",
                                directory: directory, size: directory ? nil : 4, modified: nil, node: 1)
            if directory { try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false) }
            else { try Data("old!".utf8).write(to: url) }
            var identity = stat(); precondition(lstat(url.path, &identity) == 0)
            try FileManager.default.moveItem(at: url, to: saved)
            if directory { try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false) }
            else { try Data("user".utf8).write(to: url) }
            do { _ = try removeReceivedNative(url, record: record, identity: identity, in: root) }
            catch { /* Changed native identity defers this cleanup pass. */ }
            precondition(FileManager.default.fileExists(atPath: url.path), "age cleanup removed a replacement")
            precondition(FileManager.default.fileExists(atPath: saved.path), "age cleanup removed a moved target")
            if !directory { let bytes = try Data(contentsOf: url); precondition(bytes == Data("user".utf8)) }
            let entry = try readReceived("replace", in: root)[record.id]
            precondition(entry?.deleted != true)
        }
    }
    static func pinningSurvivesOldOverlay(in root: URL) throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let file = Record(id: "pin-1", parent: "pin-root", name: "file", directory: false, size: 4,
                          modified: nil, node: 1, version: 5)
        let parent = Record(id: "pin-root", parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
                            name: "batch", directory: true, size: nil, modified: nil, node: nil, children: 1)
        func write(_ record: Record) throws {
            try JSONEncoder().encode(Catalog(token: "", port: 0, records: [parent, record], received_at: 1))
                .write(to: root.appendingPathComponent("pin.json"))
        }
        try write(file)
        precondition(Item(file).contentPolicy == .downloadLazily, "publication must not hydrate the paste")
        try receivedHandoff(file, in: root)
        let pinned = file.retainingLocal(true)
        try write(pinned)
        let overlay = try receivedOverlay(pinned, in: root)!
        precondition(overlay.keep_local == true)
        let current = try catalogs(in: root).flatMap { $0.records }.first { $0.id == file.id }!
        precondition(Item(current).contentPolicy == .downloadEagerlyAndKeepDownloaded)
        precondition(Item(current).itemVersion.contentVersion == Item(file).itemVersion.contentVersion)
        precondition(Item(current).itemVersion.metadataVersion != Item(file).itemVersion.metadataVersion)
        let renamed = Record(id: file.id, parent: file.parent, name: "renamed", directory: false,
                             size: file.size, modified: nil, node: file.node, version: file.version)
        let modified = try receivedModify(Item(renamed), fields: [.filename], contents: nil, in: root)
        precondition(modified.keep_local == true)
    }
    static func localEditRejectsLateSourceHandoff(in root: URL) throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let file = Record(id: "edited-1", parent: "edited-root", name: "file", directory: false,
                          size: 4, modified: nil, node: 1, version: 5)
        let parent = Record(id: "edited-root", parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
                            name: "batch", directory: true, size: nil, modified: nil, node: nil, children: 1)
        try JSONEncoder().encode(Catalog(token: "", port: 0, records: [parent, file], received_at: 1))
            .write(to: root.appendingPathComponent("edited.json"))
        let contents = root.appendingPathComponent("edited-content")
        try Data("edited".utf8).write(to: contents)
        let incoming = Record(id: file.id, parent: file.parent, name: file.name, directory: false,
                              size: 6, modified: nil, node: 1, version: 5)
        try receivedBegin(file, in: root)
        let edited = try assertCacheMutation { try receivedModify(Item(incoming), fields: [.contents], contents: contents, in: root) }
        precondition(edited.version == 6 && edited.size == 6 && edited.keep_local == true)
        do {
            try receivedHandoff(file, in: root)
            preconditionFailure("late source fetch overwrote a locally edited item")
        } catch {}
        do {
            try receivedFetchFailed(file.id, in: root)
            preconditionFailure("late source failure could still report noSuchItem for local contents")
        } catch {}
        do {
            try receivedBegin(file, in: root)
            preconditionFailure("source fetch began after the item became local")
        } catch {}
        let recovered = try receivedOverlay(file, in: root)
        precondition(recovered == edited, "restart lost the local version or keep-downloaded policy")
        let entry = try readReceived("edited", in: root)[file.id]!
        precondition(entry.modified_locally == true && entry.materialized && entry.handed_off)
        precondition(entry.fetching == 0)
        let bytes = try Data(contentsOf: contents)
        precondition(bytes == Data("edited".utf8))
    }
    static func emptyContainers(in root: URL) throws {
        try FileManager.default.createDirectory(at:root,withIntermediateDirectories:true)
        let source=Record(id:"source123",parent:NSFileProviderItemIdentifier.rootContainer.rawValue,name:"Peer",directory:true,size:nil,modified:1,node:nil)
        let parent=Record(id:"empty-root",parent:source.id,name:"2026-10-23 01.00.00",directory:true,size:nil,modified:1,node:nil,children:1)
        try JSONEncoder().encode(Catalog(token:"",port:0,records:[source])).write(to:root.appendingPathComponent("source123.json"))
        try JSONEncoder().encode(Catalog(token:"",port:0,records:[parent],received_at:1)).write(to:root.appendingPathComponent("empty.json"))
        try receivedHandoff(parent,in:root)
        let protected=try receivedProtected("empty",in:root)
        precondition(!protected,"enumerated synthetic parent must not pin expired delivery")
        let empty=try CatalogSnapshot.capture(in:root)
        precondition(empty.records.isEmpty,"empty timestamp/source containers leaked into working set")
        // An actual empty directory selected by the sender remains a user item.
        let directory=Record(id:"empty-1",parent:parent.id,name:"User empty folder",directory:true,size:nil,modified:2,node:1,children:0)
        try JSONEncoder().encode(Catalog(token:"",port:0,records:[parent,directory],received_at:1)).write(to:root.appendingPathComponent("empty.json"))
        let received=try CatalogSnapshot.capture(in:root)
        precondition(Set(received.records.map(\.id))==Set([source.id,parent.id,directory.id]))
        try receivedHandoff(directory,in:root)
        let delivered=try receivedProtected("empty",in:root);precondition(delivered)
        try receivedDelete([directory.id],in:root)
        let expired=try CatalogSnapshot.capture(in:root)
        precondition(expired.records.isEmpty,"last child deletion/expiry retained empty ancestors")
        precondition(Set(expired.changes(since:received,in:.workingSet).deleted.map(\.rawValue))==Set([source.id,parent.id,directory.id]))
    }
    static func privatePreparationCannotOverridePublication(in root: URL) throws {
        let old = Record(id: "typed-root", parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
                         name: ".Oink-prepared-typed.noindex", directory: true, size: nil,
                         modified: nil, node: nil, hidden: true)
        precondition(!receivedRecordEligible(old), "private preparation became eligible for received accounting")
        let final = Record(id: old.id, parent: "source", name: "received drag", directory: true,
                           size: nil, modified: nil, node: nil)
        let child = Record(id:"typed-1",parent:final.id,name:"payload",directory:false,size:1,modified:1,node:1)
        let stale = ReceivedEntry(record: old, received_at: 1000, handed_off: false, materialized: true, deleted: false)
        try JSONEncoder().encode([old.id: stale]).write(to: root.appendingPathComponent("typed.received"))
        try JSONEncoder().encode(Catalog(token: "", port: 0, records: [final,child], received_at: 1000))
            .write(to: root.appendingPathComponent("typed.json"))
        let published = try catalogs(in: root).flatMap { $0.records }.first { $0.id == old.id }
        // Enumeration adds the persisted receipt date to the final record.
        precondition(published == final.withCreationDate(1000), "private materialization overlay hid the final typed-parent rename")
        let lookup = try receivedOverlay(final, in: root)
        precondition(lookup == final, "direct item lookup returned the private prepared parent")
        let protected = try receivedProtected("typed", in: root)
        precondition(!protected, "private prepared materialization prevented retirement")
    }

    static func testReceivedHierarchyAndDates(in root: URL) throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let received: Int64 = 1_761_000_000_123
        let modified: Int64 = 1_700_000_000_456
        let source = "source" + String(repeating: "1", count: 64)
        let batch = String(repeating: "e", count: 64)
        let sourceRecord = Record(id: source, parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
            name: "Peer 111111111111", directory: true, size: nil, modified: received, node: nil)
        let parent = Record(id: batch + "-root", parent: source, name: "20251021-123456-123",
            directory: true, size: nil, modified: received, node: nil)
        let file = Record(id: batch + "-1", parent: parent.id, name: "file.txt", directory: false,
            size: 4, modified: modified, node: 1, version: 1)
        let unknown = Record(id: batch + "-2", parent: parent.id, name: "unknown.txt", directory: false,
            size: 4, modified: nil, node: 2, version: 1)
        try JSONEncoder().encode(Catalog(token: "", port: 0, records: [sourceRecord]))
            .write(to: root.appendingPathComponent(source + ".json"))
        try JSONEncoder().encode(Catalog(token: "", port: 0, records: [parent, file, unknown], received_at: Double(received)))
            .write(to: root.appendingPathComponent(batch + ".json"))
        let before = try CatalogSnapshot.capture(in: root)
        precondition(before.scoped(to: .rootContainer).map(\.name) == [sourceRecord.name])
        precondition(before.scoped(to: NSFileProviderItemIdentifier(source)).map(\.id) == [parent.id])
        let records = Dictionary(uniqueKeysWithValues: before.records.map { ($0.id, $0) })
        let exported: NSFileProviderItem = Item(records[file.id]!)
        precondition((exported.creationDate ?? nil) == Date(timeIntervalSince1970: Double(received) / 1000))
        precondition((exported.contentModificationDate ?? nil) == Date(timeIntervalSince1970: Double(modified) / 1000))
        let fallback: NSFileProviderItem = Item(records[unknown.id]!)
        precondition((fallback.contentModificationDate ?? nil) == (fallback.creationDate ?? nil))
        precondition((fallback.creationDate ?? nil) != nil)
        for id in [source, parent.id] {
            let folder: NSFileProviderItem = Item(records[id]!)
            precondition((folder.creationDate ?? nil) != nil && (folder.contentModificationDate ?? nil) != nil)
        }
        precondition(Item(records[file.id]!).itemVersion.contentVersion == Item(file).itemVersion.contentVersion,
                     "receipt dates must not replace the source content version")
        precondition(Item(records[file.id]!).itemVersion.metadataVersion != Item(file).itemVersion.metadataVersion)
        let after = try CatalogSnapshot.capture(in: root)
        precondition(after.anchor == before.anchor, "date fallback changed while idle")
        let restart = try CatalogSnapshot.load(before.anchor, in: root)
        precondition(restart.records == before.records)
        let renamed = Record(id: file.id, parent: file.parent, name: "renamed.txt", directory: false,
            size: file.size, modified: file.modified, node: file.node, version: file.version)
        let changed = try receivedModify(Item(renamed), fields: [.filename], contents: nil, in: root)
        precondition(changed.created == received && changed.modified == modified)
        precondition(changed.id == file.id && changed.parent == file.parent)
        let selected = try JSONDecoder().decode(CurrentMetadata.self,
            from: Data(#"{"schema":1,"size":4,"modified":1700000000456,"version":1}"#.utf8))
        precondition(changed.updating(selected).created == changed.created)
        // Existing source containers get a real, stable date immediately,
        // without a domain reset or moving their published paths.
        let undated = Record(id: source, parent: sourceRecord.parent, name: sourceRecord.name,
            directory: true, size: nil, modified: nil, node: nil)
        let sourceURL = root.appendingPathComponent(source + ".json")
        try JSONEncoder().encode(Catalog(token: "", port: 0, records: [undated])).write(to: sourceURL)
        let born = try sourceURL.resourceValues(forKeys: [.creationDateKey]).creationDate!
        let repaired = try CatalogSnapshot.capture(in: root)
        let sourceItem: NSFileProviderItem = Item(repaired.records.first { $0.id == source }!)
        let expected = Date(timeIntervalSince1970: Double(Int64(born.timeIntervalSince1970 * 1000)) / 1000)
        precondition((sourceItem.creationDate ?? nil) == expected)
        precondition((sourceItem.contentModificationDate ?? nil) == expected)
        let repeated = try CatalogSnapshot.capture(in: root)
        precondition(repeated.anchor == repaired.anchor)
        let saved = ReceivedEntry(record: undated, received_at: 0, handed_off: false, materialized: false, deleted: false)
        try JSONEncoder().encode([source: saved]).write(to: root.appendingPathComponent(source + ".received"))
        let overlaid = try CatalogSnapshot.capture(in: root)
        precondition(overlaid.anchor == repaired.anchor, "an undated overlay hid the source creation date")
        let spareKey = String(repeating: "f", count: 64)
        let spare = Record(id: spareKey + "-root", parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
            name: spareKey, directory: true, size: nil, modified: nil, node: nil, version: 1, children: 0)
        try JSONEncoder().encode(Catalog(token: "private", port: 0, records: [spare]))
            .write(to: root.appendingPathComponent(spareKey + ".json"))
        let stock = try CatalogSnapshot.capture(in: root)
        let hidden = stock.records.first { $0.id == spare.id }!
        precondition(hidden.hidden == true && hidden.name == spare.name && hidden.parent == spare.parent)
        precondition(Item(hidden).itemVersion.metadataVersion != Item(spare).itemVersion.metadataVersion)
        precondition(stock.scoped(to: .rootContainer).filter { !Item($0).fileSystemFlags.contains(.hidden) }.map(\.id) == [source])
    }
    static func directoryHandoffUsesReceivedVersion(in root: URL) throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let old = Record(id: "batch-1", parent: "batch-root", name: "empty", directory: true, size: nil, modified: nil, node: 1, version: 1)
        let current = Record(id: old.id, parent: old.parent, name: old.name, directory: true, size: nil, modified: nil, node: 1, version: 9, children: 0)
        try JSONEncoder().encode(Catalog(token: "", port: 0, records: [current], received_at: 1)).write(to: root.appendingPathComponent("batch.json"))
        try receivedDirectoryHandoff(old, in: root)
        let received = try readReceived("batch", in: root)[old.id]!
        precondition(received.handed_off && received.record.version == 9 && received.record.children == 0)
    }

}
