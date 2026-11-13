import XCTest
@testable import OinkAudioTestHost

/// Share / open-with device picker (native-rewrite.md「分享 / 打开方式先选设备」, review 2026-11-11).
@MainActor
final class SharePickerTests: XCTestCase {
    private let office = SendTarget(id: "office", name: "Office-PC", isComputer: true, os: "windows")
    private let macbook = SendTarget(id: "macbook", name: "MacBook-Pro", isComputer: true, os: "macos")
    private let android = SendTarget(id: "android", name: "Android", isComputer: false, os: "android")
    private let iphone = SendTarget(id: "iphone", name: "iPhone", isComputer: false, os: "ios")
    private let ipad = SendTarget(id: "ipad", name: "iPad", isComputer: false, os: "ipados")

    func testShareSizeCountsEveryPortableTextFormat() async throws {
        let provider = NSItemProvider(object: "汉" as NSString)
        let html = Data("<b>汉</b>".utf8)
        provider.registerDataRepresentation(forTypeIdentifier: "public.html", visibility: .all) { reply in
            reply(html, nil)
            return nil
        }
        let bytes = await ShareStaging.knownBytes([provider])
        XCTAssertEqual(bytes, UInt64("汉".utf8.count + html.count))
    }

    func testOpensWithTheCachedDevicesWhileSearching() {
        let model = SharePickerModel(cached: [office, macbook, android, iphone], chinese: true)
        XCTAssertTrue(model.searching)
        XCTAssertFalse(model.isEmpty)
        XCTAssertEqual(model.computers.map(\.id), ["office", "macbook"])
        XCTAssertEqual(model.phones.map(\.id), ["android", "iphone"])
        XCTAssertTrue(model.rows.allSatisfy { !$0.confirmed })
        // Seen online during the round: confirmed in place; a new one joins.
        model.update(online: [android, ipad], roundFinished: false)
        XCTAssertEqual(model.rows.map(\.id), ["office", "macbook", "android", "iphone", "ipad"])
        XCTAssertEqual(model.rows.filter(\.confirmed).map(\.id), ["android", "ipad"])
    }

    func testAfterTheRoundOnlyTheOnlineDevicesStay() {
        let model = SharePickerModel(cached: [office, macbook, android, iphone], chinese: true)
        model.update(online: [office, ipad, android], roundFinished: true)
        XCTAssertFalse(model.searching)
        XCTAssertEqual(model.computers.map(\.id), ["office"])
        XCTAssertEqual(model.phones.map(\.id), ["ipad", "android"])
        XCTAssertTrue(model.rows.allSatisfy(\.confirmed))
        model.update(online: [], roundFinished: true)
        XCTAssertTrue(model.isEmpty)
    }

    func testAnOnlineDeviceStartsAtOnce() async {
        let model = SharePickerModel(cached: [office], chinese: true)
        model.update(online: [office], roundFinished: true)
        var asked = false
        let picked = await model.pick("office") { _ in asked = true; return false }
        XCTAssertEqual(picked, office)
        XCTAssertFalse(asked, "a confirmed device is not checked again")
    }

    func testACachedDeviceIsConfirmedFirst() async {
        let model = SharePickerModel(cached: [office, macbook], chinese: true)
        let picked = await model.pick("office") { device in
            // While it waits the row is busy and other taps do nothing.
            XCTAssertEqual(model.connecting, device.id)
            let second = await model.pick("macbook") { _ in XCTFail("busy"); return true }
            XCTAssertNil(second)
            return true
        }
        XCTAssertEqual(picked, office)
        XCTAssertNil(model.connecting)
        XCTAssertNil(model.notice)
    }

    func testANotOnlineDeviceIsDroppedWithAHint() async {
        let model = SharePickerModel(cached: [office, macbook], chinese: true)
        let picked = await model.pick("macbook") { _ in false }
        XCTAssertNil(picked)
        XCTAssertNil(model.connecting)
        XCTAssertEqual(model.notice?.text, "MacBook-Pro 不在线")
        XCTAssertEqual(model.rows.map(\.id), ["office"])
        // It stays out even when a later list still has it.
        model.update(online: [macbook], roundFinished: false)
        XCTAssertEqual(model.rows.map(\.id), ["office"])
        let english = SharePickerModel(cached: [macbook], chinese: false)
        _ = await english.pick("macbook") { _ in false }
        XCTAssertEqual(english.notice?.text, "MacBook-Pro is offline")
    }

    /// The group's only other device: the page under the card lists it alone (user
    /// 2026-11-12), 「上次在线」while it is offline and「在线」once the session sees it.
    func testTheOnlyDeviceIsListedAloneUnderTheCard() {
        let model = SharePickerModel(cached: [office, macbook, android], chinese: true)
        var offline = macbook
        offline.online = false
        model.showOnly(offline)
        XCTAssertEqual(model.rows, [SharePickerModel.Row(device: offline, confirmed: false)])
        model.update(online: [], roundFinished: true)
        XCTAssertFalse(model.isEmpty, "not the empty state while the share waits for it")
        XCTAssertEqual(model.rows.map(\.id), ["macbook"])
        XCTAssertFalse(model.rows[0].confirmed)
        // It comes online: 在线, and no other device joins the page.
        model.update(online: [macbook, office], roundFinished: true)
        XCTAssertEqual(model.rows, [SharePickerModel.Row(device: macbook, confirmed: true)])
        XCTAssertEqual(model.computers.map(\.id), ["macbook"])
        XCTAssertTrue(model.phones.isEmpty)
    }

    /// The picker rows draw `PlatformLogo` inside OinkShare.appex, so the extension must
    /// ship the catalog with those images. Bug 2026-11-12: project.yml listed it under a
    /// target `resources:` key, which XcodeGen ignores, so the extension had no
    /// Assets.car and every row lost its logo.
    func testTheShareExtensionShipsThePlatformLogos() throws {
        let ios = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
        // Every image PlatformLogo draws (Shared/Icons.swift), for all platforms.
        let logos = ["PCOS-mac", "PCOS-win", "PCOS-other", "OSAndroid-head", "OSAndroid-eyes",
                     "OSAndroid-crown", "OSAndroid-crown-edge", "OSCorner-iphone", "OSCorner-ipad"]
        let catalog = ios.appendingPathComponent("App/Assets.xcassets")
        for name in logos {
            let set = catalog.appendingPathComponent("UI/\(name).imageset/Contents.json")
            XCTAssertTrue(FileManager.default.fileExists(atPath: set.path), "\(name) is not in App/Assets.xcassets")
        }
        // The generated project (xcodegen writes it next to project.yml) copies that
        // catalog into the extension.
        let project = try String(contentsOf: ios.appendingPathComponent("Oink.xcodeproj/project.pbxproj"), encoding: .utf8)
        let resources = try XCTUnwrap(Self.resourcesPhase(of: "OinkShare", in: project), "OinkShare has no Resources phase")
        XCTAssertTrue(resources.contains("/* Assets.xcassets in Resources */"), "OinkShare does not copy Assets.xcassets")
    }

    /// The `files` list of `target`'s Resources build phase in a project.pbxproj.
    private static func resourcesPhase(of target: String, in project: String) -> Substring? {
        guard let start = project.range(of: "/* \(target) */ = {\n\t\t\tisa = PBXNativeTarget;"),
              let phases = project.range(of: "buildPhases = (", range: start.upperBound..<project.endIndex),
              let phasesEnd = project.range(of: ");", range: phases.upperBound..<project.endIndex),
              let line = project[phases.upperBound..<phasesEnd.lowerBound].split(separator: "\n")
                  .first(where: { $0.contains("/* Resources */") }),
              let id = line.split(separator: " ").first?.trimmingCharacters(in: .whitespaces),
              let phase = project.range(of: "\t\t\(id) /* Resources */ = {"),
              let files = project.range(of: "files = (", range: phase.upperBound..<project.endIndex),
              let filesEnd = project.range(of: ");", range: files.upperBound..<project.endIndex) else { return nil }
        return project[files.upperBound..<filesEnd.lowerBound]
    }
}
