import Foundation
import FileProvider
@main struct ReceivedTests {
    static func main() throws {
        let root=FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at:root,withIntermediateDirectories:true)
        defer { try? FileManager.default.removeItem(at:root) }
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
        print("received deletion, handoff and restart tests passed")
    }
}
