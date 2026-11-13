import XCTest
@testable import OinkAudioTestHost

/// Group state and pairing records are excluded from backup (and so from
/// device-to-device migration); transfer history is not.
final class DeviceOnlyStorageTests: XCTestCase {
    private var root: URL!

    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: root)
    }

    /// The item's own flag, read through a fresh URL (values are cached per URL).
    private func excludedFlag(_ url: URL) throws -> Bool {
        try URL(fileURLWithPath: url.path).resourceValues(forKeys: [.isExcludedFromBackupKey])
            .isExcludedFromBackup ?? false
    }

    /// Backed up unless the item or a directory above it (up to the data
    /// root) is excluded.
    private func backedUp(_ url: URL) throws -> Bool {
        let top = root.standardizedFileURL.path
        var current = url.standardizedFileURL
        while current.path.hasPrefix(top) {
            if try excludedFlag(current) { return false }
            if current.path == top { break }
            current.deleteLastPathComponent()
        }
        return true
    }

    private func inode(_ url: URL) throws -> Int {
        try XCTUnwrap(FileManager.default.attributesOfItem(atPath: url.path)[.systemFileNumber] as? Int)
    }

    private var config: MobileConfig {
        MobileConfig(dataDir: root.path, cacheDir: nil, logDir: nil, deviceName: "Backup test",
            platform: .ios, profile: .mobile)
    }

    func testPrepareMarksTheDirectoryAgainEveryTime() throws {
        var directory = try DeviceOnlyStorage.prepare(in: root)
        XCTAssertEqual(directory.lastPathComponent, "device-only")
        XCTAssertTrue(try excludedFlag(directory))
        var values = URLResourceValues()
        values.isExcludedFromBackup = false
        try directory.setResourceValues(values)
        XCTAssertFalse(try excludedFlag(directory))
        try DeviceOnlyStorage.prepare(in: root)
        XCTAssertTrue(try excludedFlag(directory))
        XCTAssertTrue(try backedUp(root))
    }

    /// The core writes the group state itself, then replaces it atomically
    /// (temporary file + rename, a new inode); both stay excluded.
    func testGroupStateWrittenByTheCoreStaysExcludedAfterAtomicReplacement() throws {
        let directory = try DeviceOnlyStorage.prepare(in: root)
        XCTAssertEqual(try initializeMobileGroup(config: config, identitySeed: Data(repeating: 7, count: 32)), .created)
        let state = directory.appendingPathComponent("group-state.v2")
        XCTAssertTrue(FileManager.default.fileExists(atPath: state.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent("group-state.v2").path))
        XCTAssertFalse(try backedUp(state))
        let first = try inode(state)

        // Another identity, as after a migration: the core replaces the state.
        XCTAssertEqual(try initializeMobileGroup(config: config, identitySeed: Data(repeating: 9, count: 32)), .replaced)
        XCTAssertNotEqual(try inode(state), first)
        XCTAssertFalse(try backedUp(state))
        XCTAssertTrue(try excludedFlag(directory))
    }

    /// A running session writes history in the data root, which is backed up,
    /// and pairing records and the custom device name next to the group state,
    /// which is not (a new iPhone starts with its own system name).
    func testSessionKeepsHistoryBackedUpAndPairingRecordsAndNameExcluded() async throws {
        let directory = try DeviceOnlyStorage.prepare(in: root)
        let seed = Data(repeating: 7, count: 32)
        _ = try initializeMobileGroup(config: config, identitySeed: seed)
        let session = try await MobileSession.start(config: config, discovery: MobileDiscovery(), identitySeed: seed)
        _ = try await session.block(request: "1", device: String(repeating: "ab", count: 32))
        XCTAssertEqual(try session.setLocalDeviceName(name: "Desk phone").name, "Desk phone")
        try await session.shutdown()

        let history = root.appendingPathComponent("transfer.sqlite")
        XCTAssertTrue(FileManager.default.fileExists(atPath: history.path))
        XCTAssertTrue(try backedUp(history))
        let blocks = directory.appendingPathComponent("pairing-blocks.json")
        XCTAssertTrue(FileManager.default.fileExists(atPath: blocks.path))
        XCTAssertFalse(try backedUp(blocks))
        let name = directory.appendingPathComponent("device-name.json")
        XCTAssertTrue(FileManager.default.fileExists(atPath: name.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent("device-name.json").path))
        XCTAssertFalse(try backedUp(name))
    }
}
