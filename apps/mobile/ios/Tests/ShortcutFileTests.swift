import XCTest
@testable import OinkAudioTestHost

/// 一键复制: the bundled shortcut is handed to Shortcuts as 「Oink 复制.shortcut」
/// (Shortcuts names the imported shortcut after the file).
final class ShortcutFileTests: XCTestCase {
    private var root: URL!

    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: root)
    }

    func testEachAccentHasItsVariant() {
        XCTAssertEqual(ShortcutFile.resource(accent: "pink"), "OinkCopy-red")
        XCTAssertEqual(ShortcutFile.resource(accent: "orange"), "OinkCopy-darkOrange")
        XCTAssertEqual(ShortcutFile.resource(accent: "violet"), "OinkCopy-violet")
        XCTAssertEqual(ShortcutFile.resource(accent: "blue"), "OinkCopy-darkBlue")
        XCTAssertEqual(ShortcutFile.resource(accent: "teal"), "OinkCopy-teal")
        XCTAssertNil(ShortcutFile.resource(accent: "green"))
    }

    func testStagesACopyUnderTheShortcutName() throws {
        let source = root.appendingPathComponent("OinkCopy-red.shortcut")
        try Data("AEA1 signed".utf8).write(to: source)
        let first = try ShortcutFile.stage(source, in: root)
        XCTAssertEqual(first.lastPathComponent, "Oink 复制.shortcut")
        XCTAssertEqual(try Data(contentsOf: first), try Data(contentsOf: source))
        XCTAssertTrue(FileManager.default.fileExists(atPath: source.path), "the bundled file stays")

        // A second tap replaces the first copy.
        let second = try ShortcutFile.stage(source, in: root)
        XCTAssertNotEqual(first.deletingLastPathComponent(), second.deletingLastPathComponent())
        XCTAssertFalse(FileManager.default.fileExists(atPath: first.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: second.path))
    }
}
