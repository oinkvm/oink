import Foundation

/// The files that only mean something on this iPhone: the core's group state,
/// pairing journal and block list, member addresses and audio claims
/// (`crates/oink-mobile-ffi/src/device_only.rs`), and the host's cached device
/// list. They all name the Keychain identity, which is `ThisDeviceOnly` and
/// never leaves this iPhone, so they must not leave it either: a new iPhone
/// starts unpaired instead of inheriting a group whose local member it is not.
/// The core's custom device name is kept here too, so a new iPhone starts
/// with its own system name.
///
/// The whole directory is excluded from backup, which iOS also applies to
/// device-to-device migration. Excluding the directory rather than each file
/// keeps everything the core writes there excluded, including its atomic
/// replacements (temporary file + rename), which would drop a per-file flag.
/// Transfer history and settings live outside it and are backed up.
enum DeviceOnlyStorage {
    /// Must match `DIR` in `device_only.rs` (the core writes there itself).
    static let directoryName = "device-only"

    /// Creates the directory if needed and marks it excluded from backup.
    /// Idempotent; call before every core start so the flag is in place before
    /// the core writes into the directory.
    @discardableResult
    static func prepare(in root: URL) throws -> URL {
        var url = root.appendingPathComponent(directoryName, isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true,
            attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication])
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try url.setResourceValues(values)
        return url
    }
}
