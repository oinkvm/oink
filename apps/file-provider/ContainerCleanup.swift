import Foundation
import Darwin

// Keep the system-owned root and container metadata. removeItem unlinks symlinks
// rather than following them into Documents/Desktop or another app's container.
func clearContainerContents(at root: URL) throws {
    let fm = FileManager.default
    for child in try fm.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
        where !child.lastPathComponent.hasPrefix(".com.apple.") {
        try fm.removeItem(at: child)
    }
}

func clearSandboxContents(at root: URL) throws {
    let fm = FileManager.default
    for child in try fm.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
        where !child.lastPathComponent.hasPrefix(".com.apple.") {
        // Sandbox-created system preferences (not the extension's domain) are
        // protected too, including com.apple.security.plist on macOS 26.6.2.
        if root.lastPathComponent == "Preferences" &&
            (child.lastPathComponent.hasPrefix("com.apple.") || child.lastPathComponent == ".GlobalPreferences.plist") { continue }
        var info = stat()
        guard lstat(child.path, &info) == 0 else { throw CocoaError(.fileReadUnknown) }
        if info.st_mode & S_IFMT == S_IFDIR {
            // macOS 26.6.2 denies removing sandbox-created directories such as
            // Data/Library even from their owner. Recursively erase contents;
            // preserve protected directories only after their product contents
            // have been erased; only system preferences/metadata may remain.
            // lstat keeps standard home symlinks from leading outside the sandbox.
            try clearSandboxContents(at: child)
            if try fm.contentsOfDirectory(atPath: child.path).isEmpty {
                do { try fm.removeItem(at: child) }
                catch {
                    guard (error as NSError).domain == NSCocoaErrorDomain,
                          (error as NSError).code == CocoaError.fileWriteNoPermission.rawValue else { throw error }
                }
            }
        } else { try fm.removeItem(at: child) }
    }
}

func clearOwnProviderContainer() throws {
    let id = "com.hachiwii.oink.fileprovider"
    let home = FileManager.default.homeDirectoryForCurrentUser
    // Never let a bare/unsandboxed development launch erase the user's home.
    guard Bundle.main.bundleIdentifier == id,
          home.path.hasSuffix("/Library/Containers/\(id)/Data") else {
        throw CocoaError(.fileWriteNoPermission)
    }
    UserDefaults.standard.removePersistentDomain(forName: id)
    UserDefaults.standard.synchronize()
    try clearSandboxContents(at: home)
}
