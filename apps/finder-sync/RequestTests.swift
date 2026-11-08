import Foundation
@main struct RequestTests {
    static func main() throws {
        let now = ProcessInfo.processInfo.systemUptime
        let request = PasteRequest(directory: "/folder/日本語", uptime: now)
        let decoded = try JSONDecoder().decode(PasteRequest.self, from: JSONEncoder().encode(request))
        precondition(decoded.directory == request.directory && decoded.isCurrent)
        precondition(!PasteRequest(directory: "/old", uptime: now - 120).isCurrent)
        precondition(!PasteRequest(directory: "/previous-boot", uptime: now + 120).isCurrent)
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let file = root.appendingPathComponent("selected.txt")
        try Data("selected".utf8).write(to: file)
        precondition(pasteDestination(container: true, targeted: root, selected: [file]) == root)
        precondition(pasteDestination(container: false, targeted: root, selected: [root]) == root)
        precondition(pasteDestination(container: false, targeted: root, selected: [file]) == root,
                     "A single file must paste into its parent directory")
        precondition(pasteDestination(container: false, targeted: root, selected: [file, root]) == nil)
        precondition(pasteDestination(container: false, targeted: root, selected: []) == nil)
        print("8 Paste as File request/destination checks passed")
    }
}
