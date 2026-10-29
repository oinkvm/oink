import Foundation
import Darwin
import FileProvider

// Ordinary owned files exercise coordinated EOF and the real fsync path. This
// does not register a File Provider domain or claim native hydration coverage.
@main struct NativeReceivedTests {
    actor DownloadGate {
        var active = 0
        var maximum = 0
        var starts: [String] = []
        var pending: [CheckedContinuation<Void, Never>] = []
        func enter(_ id: String) async {
            starts.append(id); active += 1; maximum = max(maximum, active)
            if active == 2 {
                for waiter in pending { waiter.resume() }; pending.removeAll()
            } else {
                await withCheckedContinuation { pending.append($0) }
            }
            active -= 1
        }
        func snapshot() -> (Int, [String]) { (maximum, starts) }
    }
    static func fixture(_ id: String, parent: String = "root", directory: Bool = false) -> Record {
        Record(id: id, parent: parent, name: id, directory: directory, size: directory ? nil : 3,
               modified: nil, node: UInt64(id.suffix(1)) ?? 1, version: 1)
    }
    static func parallelTreeDownload() async throws {
        let gate = DownloadGate()
        let root = fixture("bundle", directory: true)
        var receipts: [UInt64] = []
        try await downloadReceivedTree([root], parallelism: 2, visit: { record in
            if record.directory {
                return NativeDownloadStep(children: [fixture("file1", parent: root.id), fixture("file2", parent: root.id)])
            }
            // Both independent files must reach the gate before either can
            // finish. A serial visitor cannot satisfy this dependency.
            await gate.enter(record.id)
            return NativeDownloadStep(stored: NativeStoredFile(node: record.node!, version: 1, size: 3))
        }, stored: { receipts.append($0.node) })
        let (maximum, starts) = await gate.snapshot()
        precondition(maximum == 2 && Set(starts) == ["file1", "file2"])
        precondition(Set(receipts) == [1, 2] && receipts.count == 2)
    }
    static func treeDownloadFailure() async throws {
        var receipts = 0
        for duplicate in [false, true] {
            do {
                let root = fixture("bundle", directory: true)
                try await downloadReceivedTree([root], visit: { record in
                    if duplicate { return NativeDownloadStep(children: [record]) }
                    throw CocoaError(.fileReadCorruptFile)
                }, stored: { _ in receipts += 1 })
                preconditionFailure("failed or cyclic tree accepted")
            } catch {}
        }
        precondition(receipts == 0, "a failed native read cannot emit a storage receipt")
    }
    static func publicationPathDoesNotFollowRelocation() throws {
        let directory = Record(id: "batch-1", parent: "batch-root", name: "folder", directory: true,
                               size: nil, modified: nil, node: 1, version: 1)
        let file = Record(id: "batch-2", parent: "batch-1", name: "file", directory: false,
                          size: 3, modified: nil, node: 2, version: 1)
        let catalog = Catalog(token: "", port: 0, records: [directory, file], urls: ["/private/fixture/original"])
        let expected = try receivedPublicationPath(file, catalog: catalog, batch: "batch")
        precondition(expected.path == "/private/fixture/original/file")
        precondition(expected != URL(fileURLWithPath: "/private/fixture/target/file"))
    }
    static func main() async throws {
        try await parallelTreeDownload()
        try await treeDownloadFailure()
        try publicationPathDoesNotFollowRelocation()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        try reportedMoveDeletesOwnedLocalEdits(in: root.appendingPathComponent("move"))
        try moveRejectsReplacement(in: root.appendingPathComponent("move-replacement"))
        try executableModes(in: root.appendingPathComponent("executable-modes"))
        let activity = NativeActivity()
        for size in [0, 3, 2 * 1024 * 1024 + 17] {
            let url = root.appendingPathComponent("fixture")
            let original = Data(repeating: 0x5a, count: size)
            try original.write(to: url)
            let count = try readNative(url, offset: 0, output: false, activity: activity)
            precondition(count == UInt64(size))
            let saved = try Data(contentsOf: url)
            precondition(saved == original)
            let children = try FileManager.default.contentsOfDirectory(atPath: root.path)
            precondition(Set(children) == Set(["fixture", "move", "move-replacement", "executable-modes"]))
            let policy = getiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_THREAD)
            let record = Record(id: "batch-1", parent: "batch-root", name: "fixture", directory: false,
                                size: UInt64(size), modified: nil, node: 1, version: 7)
            let resident = try syncResidentNative(url, record: record)
            precondition(resident?.size == UInt64(size) && resident?.version == 7)
            let verified = try readNative(url, offset: 0, output: false, activity: activity, residentOnly: true)
            precondition(verified == UInt64(size))
            precondition(getiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_THREAD) == policy)
            let stale = Record(id: record.id, parent: record.parent, name: record.name, directory: false,
                               size: UInt64(size + 1), modified: nil, node: 1, version: 7)
            do { _ = try syncResidentNative(url, record: stale); preconditionFailure("stale length accepted") } catch {}
            precondition(getiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_THREAD) == policy)
            if size > 0 {
                let end = try readNative(url, offset: UInt64(size - 1), output: false, activity: activity)
                precondition(end == UInt64(size))
            }
        }
        let directory = Record(id: "batch-2", parent: "batch-root", name: "directory", directory: true,
                               size: nil, modified: nil, node: 2, version: 7, children: 0)
        _ = try syncResidentNative(root, record: directory)
        let symlink = root.appendingPathComponent("redirect")
        try FileManager.default.createSymbolicLink(at: symlink, withDestinationURL: root)
        do { _ = try syncResidentNative(symlink, record: directory); preconditionFailure("redirect accepted") } catch {}
        let linkURL = root.appendingPathComponent("safe-link")
        let target = "missing-internal-target"
        try FileManager.default.createSymbolicLink(atPath: linkURL.path, withDestinationPath: target)
        let link = Record(id: "batch-3", parent: "batch-2", name: "safe-link", directory: false,
                          size: UInt64(target.utf8.count), modified: nil, node: 3, version: 9, symlink_target: target)
        let item = Item(link)
        precondition(item.contentType == .symbolicLink && item.symlinkTargetPath == target)
        let decodedLink = try JSONDecoder().decode(Record.self, from: JSONEncoder().encode(link))
        precondition(decodedLink == link)
        precondition(item.itemVersion != Item(link.withCreationDate(1).retainingLocal(true)).itemVersion)
        let residentLink = try syncResidentNative(linkURL, record: link)
        precondition(residentLink?.size == UInt64(target.utf8.count) && residentLink?.version == 9)
        let wrong = Record(id: link.id, parent: link.parent, name: link.name, directory: false,
                           size: 5, modified: nil, node: 3, version: 9, symlink_target: "other")
        do { _ = try syncResidentNative(linkURL, record: wrong); preconditionFailure("changed link accepted") } catch {}
        for (target, depth, expected) in [("nested", 0, true), ("../Resources", 1, true),
                                        ("../../outside", 1, false), ("/absolute", 2, false),
                                        ("missing", 0, true), (".", 0, false), ("", 1, false)] {
            precondition(safeSymlinkTarget(target, parentDepth: depth) == expected)
        }
        let treeRoot = Record(id: "tree-1", parent: "tree-root", name: "package", directory: true,
                              size: nil, modified: nil, node: 1)
        let nested = Record(id: "tree-2", parent: treeRoot.id, name: "nested", directory: true,
                            size: nil, modified: nil, node: 2)
        let internalLink = Record(id: "tree-3", parent: nested.id, name: "back", directory: false,
                                  size: 7, modified: nil, node: 3, version: 1, symlink_target: "../file")
        try validateReceivedSymlink(internalLink, records: [treeRoot, nested, internalLink])
        let escape = Record(id: "tree-4", parent: treeRoot.id, name: "escape", directory: false,
                            size: 10, modified: nil, node: 4, version: 1, symlink_target: "../../file")
        do { try validateReceivedSymlink(escape, records: [treeRoot, escape]); preconditionFailure("root escape accepted") } catch {}
        let sibling = Record(id: "other-1", parent: "tree-root", name: "Other", directory: true, size: nil, modified: nil, node: 8)
        let cross = Record(id: "tree-cross", parent: treeRoot.id, name: "cross", directory: false, size: 13, modified: nil, node: 9, version: 1, symlink_target: "../Other/file")
        let topLink = Record(id: "top-link", parent: "tree-root", name: "alias", directory: false, size: 10, modified: nil, node: 10, version: 1, symlink_target: "Other/file")
        try FileManager.default.createDirectory(at: root.appendingPathComponent("Other"), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: root.appendingPathComponent("package"), withIntermediateDirectories: true)
        try Data("batch-target".utf8).write(to: root.appendingPathComponent("Other/file"))
        for candidate in [cross, topLink] {
            try validateReceivedSymlink(candidate, records: [treeRoot, sibling, candidate])
            let item = Item(candidate)
            precondition(item.contentType == .symbolicLink && item.symlinkTargetPath == candidate.symlink_target)
            let path = root.appendingPathComponent(candidate.id == cross.id ? "package/cross" : "alias")
            try FileManager.default.createSymbolicLink(atPath: path.path, withDestinationPath: candidate.symlink_target!)
            let resident = try syncResidentNative(path, record: candidate)
            precondition(resident?.size == candidate.size && resident?.version == candidate.version)
            let bytes = try Data(contentsOf: path)
            precondition(bytes == Data("batch-target".utf8))
        }
        precondition(!activity.expired())
        print("Native received-copy coordinated EOF, fsync, resident-only guards, stale size, symlinks, empty files and offsets passed; no provider domain")
    }
    static func reportedMoveDeletesOwnedLocalEdits(in root: URL) throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let record = Record(id: "move-1", parent: "move-root", name: "file", directory: false,
                            size: 3, modified: nil, node: 1, version: 1)
        let parent = Record(id: "move-root", parent: "root", name: "delivery", directory: true,
                            size: nil, modified: nil, node: nil, version: 1, children: 1)
        let url = root.appendingPathComponent(record.name)
        try Data("old".utf8).write(to: url)
        try JSONEncoder().encode(Catalog(token: "", port: 0, records: [parent, record], move_state: "pending"))
            .write(to: root.appendingPathComponent("move.json"))
        try Data("new user edit".utf8).write(to: url)
        let edited = try receivedModify(Item(record), fields: [.contents], contents: url, in: root)
        var identity = stat(); precondition(lstat(url.path, &identity) == 0)
        try removeMovedNative(url, record: edited, identity: identity, root: root)
        precondition(!FileManager.default.fileExists(atPath: url.path))

    }

    static func moveRejectsReplacement(in root: URL) throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let record = Record(id: "replacement-1", parent: "replacement-root", name: "file", directory: false,
                            size: 3, modified: nil, node: 1, version: 1)
        let url = root.appendingPathComponent(record.name)
        let moved = root.appendingPathComponent("outside-target")
        try Data("old".utf8).write(to: url)
        try JSONEncoder().encode(Catalog(token: "", port: 0, records: [record], move_state: "pending"))
            .write(to: root.appendingPathComponent("replacement.json"))
        var identity = stat(); precondition(lstat(url.path, &identity) == 0)
        try FileManager.default.moveItem(at: url, to: moved)
        try Data("replacement".utf8).write(to: url)
        do {
            try removeMovedNative(url, record: record, identity: identity, root: root)
            preconditionFailure("MOVE cleanup deleted a replacement at the old path")
        } catch {}
        let replacement = try Data(contentsOf: url)
        let target = try Data(contentsOf: moved)
        precondition(replacement == Data("replacement".utf8) && target == Data("old".utf8))
        try FileManager.default.removeItem(at: url)
        try Data("old".utf8).write(to: url)
        precondition(lstat(url.path, &identity) == 0)
        try assertCacheMutation { try removeMovedNative(url, record: record, identity: identity, root: root) }
        precondition(!FileManager.default.fileExists(atPath: url.path))
        let stillTarget = try Data(contentsOf: moved)
        precondition(stillTarget == target)
        let deleted = try readReceived("replacement", in: root)[record.id]
        precondition(deleted?.deleted == true)
    }

    static func executableModes(in root: URL) throws {
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let url = root.appendingPathComponent("executable")
        try Data("payload".utf8).write(to: url)
        for bits: UInt16 in [0o111, 0o100, 0o010, 0o001, 0] {
            guard chmod(url.path, 0o7666) == 0 else { throw CocoaError(.fileWriteUnknown) }
            let fd = Darwin.open(url.path, O_RDONLY | O_NOFOLLOW); precondition(fd >= 0)
            try applyExecutableBits(bits, descriptor: fd); close(fd)
            var metadata = stat(); precondition(lstat(url.path, &metadata) == 0)
            precondition(metadata.st_mode & 0o111 == mode_t(bits) && metadata.st_mode & 0o7002 == 0)
            let record = Record(id: "exec-1", parent: "exec-root", name: "executable", directory: false,
                                size: 7, modified: nil, node: 1, version: 1, exec_bits: bits)
            let resident = try syncResidentNative(url, record: record); precondition(resident?.size == 7)
            precondition(Item(record).fileSystemFlags.contains(.userExecutable) == (bits & 0o100 != 0))
            let other = Record(id: record.id, parent: record.parent, name: record.name, directory: false,
                               size: 7, modified: nil, node: 1, version: 1, exec_bits: bits ^ 0o100)
            precondition(Item(record).itemVersion.contentVersion != Item(other).itemVersion.contentVersion)
            do { _ = try syncResidentNative(url, record: other); preconditionFailure("wrong executable bits accepted") } catch {}
        }
        let fd = Darwin.open(url.path, O_RDONLY | O_NOFOLLOW); defer { close(fd) }
        do { try applyExecutableBits(0o4755, descriptor: fd); preconditionFailure("source mode accepted") } catch {}
    }

}
