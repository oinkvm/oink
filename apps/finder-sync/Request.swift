import Foundation

func pasteDestination(container: Bool, targeted: URL?, selected: [URL]?) -> URL? {
    if container { return targeted }
    guard let selected, selected.count == 1,
          let values = try? selected[0].resourceValues(forKeys: [.isDirectoryKey, .isRegularFileKey]) else { return nil }
    if values.isDirectory == true { return selected[0] }
    return values.isRegularFile == true ? selected[0].deletingLastPathComponent() : nil
}

struct PasteRequest: Codable {
    let directory: String
    let uptime: TimeInterval
    // A menu click is ephemeral, not a job to replay after a crash/restart.
    // System uptime is shared across processes and avoids wall-clock changes.
    var isCurrent: Bool {
        let age = ProcessInfo.processInfo.systemUptime - uptime
        return age >= 0 && age < 60
    }
}
func pasteGroupContainer() -> URL? {
    guard let group = Bundle.main.object(forInfoDictionaryKey: "OinkAppGroup") as? String else { return nil }
    return FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: group)
}
func pasteRequestDirectory() -> URL? {
    pasteGroupContainer()?.appendingPathComponent("paste-as-file-requests", isDirectory: true)
}
