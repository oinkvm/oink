#if !DEBUG
#error("Uninstall tests require a debug build")
#endif
import Foundation

@main struct UninstallTests {
    static func main() throws {
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
        print("Uninstall ordering, error barriers and owned paths passed")
    }
}
