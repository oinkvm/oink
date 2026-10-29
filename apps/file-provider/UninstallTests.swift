#if !DEBUG
#error("Uninstall tests require a debug build")
#endif
import Foundation

@main struct UninstallTests {
    static func main() async throws {
        var steps: [UninstallFinishStep] = []
        try finishUninstall { steps.append($0) }
        precondition(steps == [.processes, .privacy, .data, .trash])
        for failure in steps {
            var seen: [UninstallFinishStep] = []
            do {
                try finishUninstall { step in
                    seen.append(step)
                    if step == failure { throw CocoaError(.fileWriteNoPermission) }
                }
                preconditionFailure("failed cleanup acknowledged")
            } catch { precondition(seen.last == failure && seen.count == steps.firstIndex(of: failure)! + 1) }
        }
        let home = URL(fileURLWithPath: "/synthetic-home")
        let paths = uninstallLocations(home: home)
        precondition(paths.contains(home.appendingPathComponent("Library/Application Support/Oink")))
        precondition(paths.contains(home.appendingPathComponent("Library/WebKit/com.hachiwii.oink.ui")))
        precondition(paths.allSatisfy { $0.path.hasPrefix(home.path + "/Library/") && !$0.path.contains("Across") })
        precondition(!paths.contains { $0.path.contains("CloudStorage") })
        precondition(!paths.contains { $0.path.contains("/Containers/") })

        // An absent/disabled domain still needs self-cleanup. Every error must
        // stop preparation before it can acknowledge removal to the UI.
        for result in ["absent", "removed"] {
            var seen: [String] = []
            let actual = try await finishUninstallProvider(removeDomains: {
                seen.append("remove"); return result
            }, stopExtension: { seen.append("stop") }, clearExtension: { seen.append("clear") })
            precondition(actual == result && seen == ["remove", "stop", "clear"])
        }
        for failure in ["remove", "stop", "clear"] {
            var seen: [String] = []
            func run(_ step: String) throws {
                seen.append(step)
                if step == failure { throw CocoaError(.fileWriteNoPermission) }
            }
            do {
                _ = try await finishUninstallProvider(removeDomains: { try run("remove"); return "removed" },
                    stopExtension: { try run("stop") }, clearExtension: { try run("clear") })
                preconditionFailure("extension cleanup failure acknowledged")
            } catch { precondition(seen == Array(["remove", "stop", "clear"].prefix(seen.count)) && seen.last == failure) }
        }

        let fm = FileManager.default
        let fixture = fm.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? fm.removeItem(at: fixture) }
        let testHome = fixture.appendingPathComponent("home")
        let group = fixture.appendingPathComponent("resolved-group")
        let outside = fixture.appendingPathComponent("outside")
        func write(_ url: URL) throws {
            try fm.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data("fixture".utf8).write(to: url)
        }
        try write(outside)
        for id in uninstallBundleIDs {
            try write(testHome.appendingPathComponent("Library/Containers/\(id)/Data/preserve"))
        }
        let metadata = group.appendingPathComponent(".com.apple.containermanagerd.metadata.plist")
        try write(metadata)
        try write(group.appendingPathComponent("FileProvider/temporary/payload"))
        try write(group.appendingPathComponent("FileProvider/Reports/capsule.bin"))
        try write(group.appendingPathComponent("LinkedFileProvider/received/journal"))
        try write(group.appendingPathComponent("LinkedFileProvider/temporary/payload"))
        try fm.createSymbolicLink(at: group.appendingPathComponent("external-link"), withDestinationURL: outside)
        for location in uninstallLocations(home: testHome) { try write(location) }
        var preferencesCleared = false
        precondition(removeOinkData(home: testHome, groupContainer: { group },
            clearPreferences: { preferencesCleared = true }).isEmpty)
        precondition(preferencesCleared)
        let groupChildren = try fm.contentsOfDirectory(atPath: group.path)
        precondition(groupChildren == [metadata.lastPathComponent])
        precondition(fm.fileExists(atPath: outside.path))
        precondition(uninstallLocations(home: testHome).allSatisfy { !fm.fileExists(atPath: $0.path) })
        for id in uninstallBundleIDs {
            precondition(fm.fileExists(atPath: testHome.appendingPathComponent("Library/Containers/\(id)/Data/preserve").path))
        }
        precondition(removeOinkData(home: testHome, groupContainer: { throw CocoaError(.fileReadNoPermission) },
            clearPreferences: {}).contains("App Group"))

        let ownHome = fixture.appendingPathComponent("own-data")
        try write(ownHome.appendingPathComponent("Library/Preferences/extension.plist"))
        try write(ownHome.appendingPathComponent("tmp/payload"))
        try write(ownHome.appendingPathComponent("Documents/payload"))
        try fm.createSymbolicLink(at: ownHome.appendingPathComponent("Desktop"), withDestinationURL: outside)
        try clearSandboxContents(at: ownHome)
        let ownChildren = try fm.contentsOfDirectory(atPath: ownHome.path)
        precondition(ownChildren.isEmpty)
        precondition(fm.fileExists(atPath: outside.path))
        let systemPreferences = ownHome.appendingPathComponent("Library/Preferences/com.apple.security.plist")
        let globalPreferences = ownHome.appendingPathComponent("Library/Preferences/.GlobalPreferences.plist")
        try write(systemPreferences)
        try write(globalPreferences)
        let productPreferences = ownHome.appendingPathComponent("Library/Preferences/com.hachiwii.oink.fileprovider.plist")
        try write(productPreferences)
        try clearSandboxContents(at: ownHome)
        precondition(fm.fileExists(atPath: systemPreferences.path) && fm.fileExists(atPath: globalPreferences.path))
        precondition(!fm.fileExists(atPath: productPreferences.path))
        do { try clearOwnProviderContainer(); preconditionFailure("unsandboxed test erased home") }
        catch { precondition((error as NSError).code == CocoaError.fileWriteNoPermission.rawValue) }
        print("Uninstall ordering, extension cleanup barriers, container isolation, symlinks and group cleanup passed")
    }
}
