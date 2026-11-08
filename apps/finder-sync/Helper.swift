import Foundation

@main struct OinkPasteHelper {
    static func main() {
        guard let inbox = pasteRequestDirectory(),
              let requests = try? FileManager.default.contentsOfDirectory(at: inbox, includingPropertiesForKeys: nil) else { return }
        let outer = Bundle.main.bundleURL.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let agent = outer.appendingPathComponent("Contents/MacOS/oink-agent")
        for request in requests where request.pathExtension == "json" {
            let claimed = inbox.appendingPathComponent(UUID().uuidString + ".claimed")
            // Atomic rename claims an action across simultaneous helper launches.
            // Remove it before spawning the CLI: interrupted clicks never replay.
            do { try FileManager.default.moveItem(at: request, to: claimed) } catch { continue }
            let bytes = try? Data(contentsOf: claimed)
            try? FileManager.default.removeItem(at: claimed)
            guard let bytes, let value = try? JSONDecoder().decode(PasteRequest.self, from: bytes), value.isCurrent else { continue }
            let process = Process()
            process.executableURL = agent
            process.arguments = ["paste-as-file", value.directory]
            do {
                try process.run()
                process.waitUntilExit()
                if process.terminationStatus != 0 { NSLog("Oink Paste as File failed: status=%d", process.terminationStatus) }
            } catch { NSLog("Oink Paste as File launch failed: %@", error.localizedDescription) }
        }
    }
}
