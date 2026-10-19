import Foundation
import AppKit
import Darwin
import ServiceManagement

enum UninstallFinishStep: String { case processes, privacy, data, trash }

func finishUninstall(run: (UninstallFinishStep) throws -> Void) throws {
    for step in [UninstallFinishStep.processes, .privacy, .data, .trash] { try run(step) }
}

struct UninstallMessages: Decodable {
    let title: String
    let failure: String
    let done: String
    let processes: String
    let privacy: String
    let data: String
    let trash: String
}
struct UninstallRequest: Decodable {
    let uiPID: pid_t
    let messages: UninstallMessages
}

// The outer backend owns input capture and microphone access. The WebView UI
// has a separate LS bundle ID. The provider only accesses its shared sandbox
// and coordinated snapshots; it never requests a TCC permission.
// The CLI and crash collector are signed tools, not independently registered
// application bundles or permission-requesting processes.
let uninstallBundleIDs = ["com.hachiwii.oink", "com.hachiwii.oink.ui", "com.hachiwii.oink.fileprovider"]

func uninstallLocations(home: URL) -> [URL] {
    var paths = ["Library/Application Support/Oink", "Library/Logs/Oink"]
    for id in uninstallBundleIDs {
        paths += ["Library/Caches/\(id)", "Library/Preferences/\(id).plist",
                  "Library/WebKit/\(id)", "Library/HTTPStorages/\(id)",
                  "Library/HTTPStorages/\(id).binarycookies", "Library/Saved Application State/\(id).savedState"]
    }
    return paths.map { home.appendingPathComponent($0) }
}

func unregisterOinkLoginItem() async throws {
    let service = SMAppService.mainApp
    if service.status == .enabled || service.status == .requiresApproval { try await service.unregister() }
    guard service.status == .notRegistered || service.status == .notFound else { throw CocoaError(.featureUnsupported) }
}

// libproc reports executable paths, including sandboxed appex processes which
// are not returned by NSWorkspace.runningApplications. Never kill by a name.
@_silgen_name("proc_listallpids") private func listPIDs(_ buffer: UnsafeMutableRawPointer?, _ size: Int32) -> Int32
@_silgen_name("proc_pidpath") private func processPath(_ pid: Int32, _ buffer: UnsafeMutableRawPointer, _ size: UInt32) -> Int32

func bundleProcesses(_ app: URL, excluding: Set<pid_t> = [], includeExtension: Bool = true) throws -> [pid_t] {
    let count = listPIDs(nil, 0)
    guard count > 0 else { throw CocoaError(.executableRuntimeMismatch) }
    var pids = [pid_t](repeating: 0, count: Int(count) + 256)
    let length = pids.withUnsafeMutableBytes { listPIDs($0.baseAddress, Int32($0.count)) }
    guard length > 0, length < pids.count else { throw CocoaError(.executableRuntimeMismatch) }
    return pids.prefix(Int(length)).filter { pid in
        guard pid > 0, pid != getpid(), !excluding.contains(pid) else { return false }
        var path = [CChar](repeating: 0, count: 4096)
        let length = path.withUnsafeMutableBytes { processPath(pid, $0.baseAddress!, UInt32($0.count)) }
        let executable = String(cString: path)
        return length > 0 && executable.hasPrefix(app.path + "/") &&
            (includeExtension || !executable.hasPrefix(app.path + "/Contents/PlugIns/"))
    }
}

func waitForOinkExit(app: URL, uiPID: pid_t) throws {
    let deadline = Date().addingTimeInterval(20)
    // The UI exits only after receiving our ready ACK. Do not terminate it
    // before that handoff or an error could silently discard the dialog.
    while kill(uiPID, 0) == 0 && Date() < deadline { Thread.sleep(forTimeInterval: 0.05) }
    guard kill(uiPID, 0) != 0 else { throw CocoaError(.executableRuntimeMismatch) }
    try stopOinkProcesses(app: app)
}

func stopOinkProcesses(app: URL, excluding: Set<pid_t> = [], includeExtension: Bool = true) throws {
    let deadline = Date().addingTimeInterval(10)
    func remaining() throws -> [pid_t] { try bundleProcesses(app, excluding: excluding, includeExtension: includeExtension) }
    for pid in try remaining() { _ = kill(pid, SIGTERM) }
    while !(try remaining()).isEmpty && Date() < deadline { Thread.sleep(forTimeInterval: 0.05) }
    // A removed File Provider domain no longer needs its extension. Only
    // processes whose current executable is inside this exact bundle qualify.
    for pid in try remaining() { _ = kill(pid, SIGKILL) }
    let finalDeadline = Date().addingTimeInterval(3)
    while !(try remaining()).isEmpty && Date() < finalDeadline { Thread.sleep(forTimeInterval: 0.05) }
    guard try remaining().isEmpty else { throw CocoaError(.executableRuntimeMismatch) }
}

func installedUninstallApp() throws -> URL {
    let app = URL(fileURLWithPath: CommandLine.arguments[0]).resolvingSymlinksInPath()
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    guard app.path == "/Applications/Oink.app", Bundle(url: app)?.bundleIdentifier == uninstallBundleIDs[0] else {
        throw CocoaError(.executableNotLoadable)
    }
    return app
}

func resetOinkPrivacy() -> [String] {
    var failed: [String] = []
    for id in uninstallBundleIDs.prefix(2) {
        // tccutil resolves IDs through LaunchServices. Keep the registered app
        // in place until all resets finish; never register a replacement here.
        guard NSWorkspace.shared.urlForApplication(withBundleIdentifier: id) != nil else { failed.append(id); continue }
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/usr/bin/tccutil")
        task.arguments = ["reset", "All", id]
        task.standardOutput = FileHandle.nullDevice
        task.standardError = FileHandle.nullDevice
        do { try task.run(); task.waitUntilExit(); if task.terminationStatus != 0 { failed.append(id) } }
        catch { failed.append(id) }
    }
    return failed
}

func removeOinkData(home: URL) -> [String] {
    let fm = FileManager.default
    var failed: [String] = []
    for id in uninstallBundleIDs {
        UserDefaults.standard.removePersistentDomain(forName: id)
    }
    UserDefaults.standard.synchronize()
    for url in uninstallLocations(home: home) where fm.fileExists(atPath: url.path) {
        do { try fm.removeItem(at: url) } catch { failed.append(url.lastPathComponent) }
    }
    // These are the same process families the diagnostics reader imports.
    // Inspect only Oink's reports; the shared DiagnosticReports directory stays.
    let reports = home.appendingPathComponent("Library/Logs/DiagnosticReports")
    if let entries = try? fm.contentsOfDirectory(at: reports, includingPropertiesForKeys: nil) {
        for report in entries where report.pathExtension == "ips" &&
            ["Oink-", "oink-", "OinkFileProvider-"].contains(where: { report.lastPathComponent.hasPrefix($0) }) {
            do { try fm.removeItem(at: report) } catch { failed.append(report.lastPathComponent) }
        }
    }
    // Remove Oink-owned contents while leaving OS-owned container metadata and
    // protected roots alone. macOS may deny access without Full Disk Access;
    // uninstall never asks for it or tries to defeat container protection.
    let containers = uninstallBundleIDs.map { home.appendingPathComponent("Library/Containers/\($0)") }
        + [home.appendingPathComponent("Library/Group Containers/893V37X2CW.com.hachiwii.oink.files")]
    for root in containers where fm.fileExists(atPath: root.path) {
        do {
            for child in try fm.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
                where !child.lastPathComponent.hasPrefix(".com.apple.") {
                do { try fm.removeItem(at: child) }
                catch { failed.append(root.lastPathComponent + "/" + child.lastPathComponent) }
            }
        } catch { failed.append(root.lastPathComponent) }
    }
    return failed
}

@MainActor func finishOinkUninstall() throws {
    let request = try JSONDecoder().decode(UninstallRequest.self, from: FileHandle.standardInput.readDataToEndOfFile())
    let app = try installedUninstallApp()
    guard request.uiPID > 1 else {
        throw CocoaError(.executableNotLoadable)
    }
    print("ready"); fflush(stdout)
    // No Oink logger/telemetry is initialized in this process. After this
    // barrier it is the sole Oink process and never creates product stores.
    let messages = request.messages
    var failures: [String] = []
    var current = UninstallFinishStep.processes
    do {
        try finishUninstall { step in
            current = step
            switch step {
            case .processes: try waitForOinkExit(app: app, uiPID: request.uiPID)
            case .privacy:
                let ids = resetOinkPrivacy()
                if !ids.isEmpty { failures.append(messages.privacy + ": " + ids.joined(separator: ", ")) }
            case .data:
                let paths = removeOinkData(home: FileManager.default.homeDirectoryForCurrentUser)
                if !paths.isEmpty { failures.append(messages.data + ": " + paths.joined(separator: ", ")) }
            case .trash: try FileManager.default.trashItem(at: app, resultingItemURL: nil)
            }
        }
    } catch {
        let label: String
        switch current { case .processes: label = messages.processes; case .privacy: label = messages.privacy; case .data: label = messages.data; case .trash: label = messages.trash }
        failures.append(label + " (\((error as NSError).code))")
    }
    if !failures.isEmpty {
        NSApplication.shared.setActivationPolicy(.accessory)
        NSApplication.shared.activate(ignoringOtherApps: true)
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = messages.title
        alert.informativeText = messages.failure + "\n\n" + failures.joined(separator: "\n")
        alert.addButton(withTitle: messages.done)
        alert.runModal()
    }
}
