import Foundation
import FileProvider
@main struct ReceivedTests {
    static func main() throws {
        let root=FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at:root,withIntermediateDirectories:true)
        defer { try? FileManager.default.removeItem(at:root) }
        try testReceivedHierarchyAndDates(in: root.appendingPathComponent("dates"))
        let record=Record(id:"batch-1",parent:"batch-root",name:"fixture",directory:false,size:4,modified:nil,node:1)
        let catalog=Catalog(token:"",port:0,records:[Record(id:"batch-root",parent:NSFileProviderItemIdentifier.rootContainer.rawValue,name:"batch",directory:true,size:nil,modified:nil,node:nil),record],received_at:1000)
        try JSONEncoder().encode(catalog).write(to:root.appendingPathComponent("batch.json"))
        try receivedHandoff(record,in:root,at:Date(timeIntervalSince1970:100))
        let stored=try readReceived("batch",in:root)[record.id]!
        precondition(stored.received_at==1000,"age starts at delivery, not first fetch")
        let protected=try receivedProtected("batch",in:root);precondition(protected)
        let original=try receivedOverlay(record,in:root);precondition(original == record)
        let renamed=Record(id:record.id,parent:record.parent,name:"renamed",directory:false,size:4,modified:nil,node:1)
        let changed=try receivedModify(Item(renamed),fields:[.filename],contents:nil,in:root)
        precondition(changed.name=="renamed")
        precondition(Item(changed).itemVersion.metadataVersion != Item(record).itemVersion.metadataVersion)
        try receivedDelete([record.id],in:root)
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
    static func privatePreparationCannotOverridePublication(in root: URL) throws {
        let old = Record(id: "typed-root", parent: NSFileProviderItemIdentifier.rootContainer.rawValue,
                         name: ".Oink-prepared-typed.noindex", directory: true, size: nil,
                         modified: nil, node: nil, hidden: true)
        precondition(!receivedRecordEligible(old), "private preparation became eligible for received accounting")
        let final = Record(id: old.id, parent: "source", name: "received drag", directory: true,
                           size: nil, modified: nil, node: nil)
        let stale = ReceivedEntry(record: old, received_at: 1000, handed_off: false, materialized: true, deleted: false)
        try JSONEncoder().encode([old.id: stale]).write(to: root.appendingPathComponent("typed.received"))
        try JSONEncoder().encode(Catalog(token: "", port: 0, records: [final], received_at: 1000))
            .write(to: root.appendingPathComponent("typed.json"))
        let published = try catalogs(in: root).flatMap { $0.records }.first { $0.id == old.id }
        precondition(published == final, "private materialization overlay hid the final typed-parent rename")
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
}
