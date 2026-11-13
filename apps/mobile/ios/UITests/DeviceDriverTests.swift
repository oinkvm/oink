import XCTest

/// Real-device UI driver for Oink (run with `xcodebuild test-without-building
/// -only-testing:OinkUITests/DeviceDriverTests/testDriver` on a physical iPhone).
///
/// The Mac drives the phone step by step without rebuilding: it copies a batch file
/// `Documents/cmd-<n>.json` into this runner's data container with
/// `xcrun devicectl device copy to --domain-type appDataContainer
/// --domain-identifier com.hachiwii.oink.ui-tests.xctrunner`, and copies back
/// `Documents/out/<n>.json` (step results) plus any screenshots the batch took.
/// `scripts/mobile/ios-device-driver.sh` wraps both directions.
///
/// A batch is `{"steps": [ {"op": ...}, ... ]}`. Steps stop at the first failure.
/// Elements are read through snapshots and tapped by coordinate: a failed XCUIElement
/// action records an XCTest failure that ends the whole driver, a snapshot only throws.
/// Secrets (pairing codes) are passed with `"textFile"`: the runner reads the file from
/// its Documents directory, deletes it and types the text, so the code never appears in
/// a command, a result or the test log.
final class DeviceDriverTests: XCTestCase {
    private let oink = XCUIApplication()
    private let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
    private lazy var docs = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    private lazy var outDir = docs.appendingPathComponent("out", isDirectory: true)

    func testDriver() throws {
        continueAfterFailure = true
        let fm = FileManager.default
        try? fm.removeItem(at: outDir)
        try fm.createDirectory(at: outDir, withIntermediateDirectories: true)
        for name in (try? fm.contentsOfDirectory(atPath: docs.path)) ?? [] where name.hasPrefix("cmd-") {
            try? fm.removeItem(at: docs.appendingPathComponent(name))
        }
        let idle = Double(ProcessInfo.processInfo.environment["OINK_DRIVER_IDLE_SECONDS"] ?? "") ?? 1800
        try Data("ready\n".utf8).write(to: outDir.appendingPathComponent("ready"))
        var lastCommand = Date()
        var done = Set<String>()
        while Date().timeIntervalSince(lastCommand) < idle {
            let pending = ((try? fm.contentsOfDirectory(atPath: docs.path)) ?? [])
                .filter { $0.hasPrefix("cmd-") && $0.hasSuffix(".json") }
                .sorted { seq($0) < seq($1) }
            guard let name = pending.first else {
                Thread.sleep(forTimeInterval: 0.2)
                continue
            }
            lastCommand = Date()
            let url = docs.appendingPathComponent(name)
            let id = String(seq(name))
            // The Mac retries a copy devicectl reported as failed; run each batch once.
            guard done.insert(id).inserted else {
                try? fm.removeItem(at: url)
                continue
            }
            var results: [[String: Any]] = []
            var quit = false
            do {
                let data = try Data(contentsOf: url)
                try? fm.removeItem(at: url)
                let batch = try JSONSerialization.jsonObject(with: data) as? [String: Any] ?? [:]
                for step in batch["steps"] as? [[String: Any]] ?? [] {
                    let started = Date()
                    var result: [String: Any] = ["op": step["op"] ?? ""]
                    do {
                        if step["op"] as? String == "quit" { quit = true; result["ok"] = true }
                        else { try run(step, id: id, into: &result); result["ok"] = true }
                    } catch {
                        result["ok"] = false
                        result["error"] = "\(error)"
                    }
                    result["ms"] = Int(Date().timeIntervalSince(started) * 1000)
                    results.append(result)
                    if result["ok"] as? Bool != true || quit { break }
                }
            } catch {
                results.append(["op": "parse", "ok": false, "error": "\(error)"])
            }
            let out = try JSONSerialization.data(withJSONObject: ["id": id, "results": results], options: [.prettyPrinted, .sortedKeys])
            // Write then rename so the Mac never copies a half-written result.
            let tmp = outDir.appendingPathComponent("\(id).json.tmp")
            try out.write(to: tmp)
            try fm.moveItem(at: tmp, to: outDir.appendingPathComponent("\(id).json"))
            if quit { return }
        }
    }

    private func seq(_ name: String) -> Int {
        Int(name.dropFirst(4).dropLast(5)) ?? Int.max
    }

    struct StepError: Error, CustomStringConvertible {
        let description: String
        init(_ text: String) { description = text }
    }

    // MARK: Steps

    private func run(_ step: [String: Any], id: String, into result: inout [String: Any]) throws {
        let op = step["op"] as? String ?? ""
        let timeout = step["timeout"] as? Double ?? 8
        switch op {
        case "launch":
            // Launch arguments let a batch start from a known state (e.g. a tab).
            oink.launchArguments = step["args"] as? [String] ?? []
            oink.launch()
        case "activate":
            app(step).activate()
        case "terminate":
            app(step).terminate()
        case "home":
            XCUIDevice.shared.press(.home)
        case "sleep":
            Thread.sleep(forTimeInterval: step["seconds"] as? Double ?? 1)
        case "shot":
            let shot = XCUIScreen.main.screenshot()
            let name = (step["name"] as? String ?? "shot") + ".png"
            try shot.pngRepresentation.write(to: outDir.appendingPathComponent(name))
            result["file"] = name
        case "tree":
            let text = app(step).debugDescription
            let name = (step["name"] as? String ?? "tree-\(id)") + ".txt"
            try Data(text.utf8).write(to: outDir.appendingPathComponent(name))
            result["file"] = name
        case "texts":
            // One snapshot of the whole tree: a single round trip that cannot race.
            let only = step["types"] as? [String]
            var rows: [String] = []
            func walk(_ e: XCUIElementSnapshot) {
                let name = typeName(e.elementType)
                let f = e.frame
                let value = (e.value as? String) ?? ""
                if only == nil || only!.contains(name), !f.isEmpty,
                   !(e.label.isEmpty && value.isEmpty && e.identifier.isEmpty) {
                    rows.append(String(format: "%@\t%.0f,%.0f %.0fx%.0f\t%@\t%@\t%@%@", name, f.minX, f.minY, f.width, f.height,
                                       e.label, value, e.identifier, e.isSelected ? "\tselected" : ""))
                }
                e.children.forEach(walk)
            }
            walk(try app(step).snapshot())
            result["elements"] = rows
        case "wait":
            result["label"] = try snapshot(step, wait: timeout).label
        case "waitGone":
            let e = query(step)
            let deadline = Date().addingTimeInterval(timeout)
            while e.exists && Date() < deadline { Thread.sleep(forTimeInterval: 0.2) }
            if e.exists { throw StepError("still present: \(describe(step))") }
        case "exists":
            result["exists"] = query(step).waitForExistence(timeout: step["timeout"] as? Double ?? 0.5)
        case "read":
            let snap = try snapshot(step, wait: timeout)
            result["label"] = snap.label
            result["value"] = (snap.value as? String) ?? ""
            result["frame"] = "\(snap.frame)"
            result["enabled"] = snap.isEnabled
            result["selected"] = snap.isSelected
        case "tap":
            try target(step, wait: timeout, into: &result).tap()
        case "longPress":
            try target(step, wait: timeout, into: &result).press(forDuration: step["seconds"] as? Double ?? 1.0)
        case "type":
            var text = step["text"] as? String ?? ""
            if let file = step["textFile"] as? String {
                let url = docs.appendingPathComponent(file)
                text = try String(contentsOf: url, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)
                try? FileManager.default.removeItem(at: url)
            }
            if step["label"] != nil || step["contains"] != nil || step["point"] != nil {
                if step["noTap"] as? Bool != true { try target(step, wait: timeout, into: &result).tap() }
                Thread.sleep(forTimeInterval: 0.4)
            }
            if let count = step["deleteCount"] as? Int, count > 0 {
                app(step).typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: count))
            }
            app(step).typeText(text)
            result["typed"] = step["textFile"] != nil ? "<secret>" : text
        case "swipe":
            // A drag through the middle of the screen (or of the matched element).
            var frame = try app(step).snapshot().frame
            if step["label"] != nil || step["contains"] != nil { frame = try snapshot(step, wait: timeout).frame }
            let c = CGPoint(x: frame.midX, y: frame.midY)
            let dy = frame.height * (step["fraction"] as? Double ?? 0.3), dx = frame.width * 0.3
            let from: CGPoint, to: CGPoint
            switch step["dir"] as? String ?? "up" {
            case "up": from = CGPoint(x: c.x, y: c.y + dy); to = CGPoint(x: c.x, y: c.y - dy)
            case "down": from = CGPoint(x: c.x, y: c.y - dy); to = CGPoint(x: c.x, y: c.y + dy)
            case "left": from = CGPoint(x: c.x + dx, y: c.y); to = CGPoint(x: c.x - dx, y: c.y)
            default: from = CGPoint(x: c.x - dx, y: c.y); to = CGPoint(x: c.x + dx, y: c.y)
            }
            drag(step, from, to, hold: 0.05)
        case "drag":
            let p = step["from"] as? [Double] ?? [0, 0], q = step["to"] as? [Double] ?? [0, 0]
            drag(step, CGPoint(x: p[0], y: p[1]), CGPoint(x: q[0], y: q[1]), hold: step["hold"] as? Double ?? 0.1)
        case "secretDigits":
            // A code shown as one digit per cell below the `contains` anchor, written to
            // Documents/out/<file> for the Mac to pull and delete; never into a result.
            let anchor = try snapshot(step, wait: timeout).frame
            var digits: [(CGFloat, String)] = []
            func walk(_ e: XCUIElementSnapshot) {
                if e.elementType == .staticText, e.label.count == 1, e.label.allSatisfy(\.isNumber),
                   e.frame.minY > anchor.maxY, e.frame.minY < anchor.maxY + 120 {
                    digits.append((e.frame.minX, e.label))
                }
                e.children.forEach(walk)
            }
            walk(try app(step).snapshot())
            let code = digits.sorted { $0.0 < $1.0 }.map(\.1).joined()
            try Data(code.utf8).write(to: outDir.appendingPathComponent(step["file"] as? String ?? "secret"))
            result["digits"] = code.count
        case "removeFile":
            try? FileManager.default.removeItem(at: outDir.appendingPathComponent(step["file"] as? String ?? "secret"))
        case "state":
            result["state"] = app(step).state.rawValue
        default:
            throw StepError("unknown op \(op)")
        }
    }

    private func drag(_ step: [String: Any], _ from: CGPoint, _ to: CGPoint, hold: Double) {
        let base = app(step).coordinate(withNormalizedOffset: .zero)
        base.withOffset(CGVector(dx: from.x, dy: from.y))
            .press(forDuration: hold, thenDragTo: base.withOffset(CGVector(dx: to.x, dy: to.y)),
                   withVelocity: .default, thenHoldForDuration: 0.05)
    }

    private func app(_ step: [String: Any]) -> XCUIApplication {
        switch step["app"] as? String {
        case nil, "oink": return oink
        case "springboard": return springboard
        case let bundle?: return XCUIApplication(bundleIdentifier: bundle)
        }
    }

    private func elementType(_ name: String?) -> XCUIElement.ElementType {
        switch name {
        case "button": .button
        case "text": .staticText
        case "field": .textField
        case "secureField": .secureTextField
        case "textView": .textView
        case "switch": .switch
        case "toggle": .toggle
        case "cell": .cell
        case "image": .image
        case "slider": .slider
        case "tab": .tab
        case "menuItem": .menuItem
        case "link": .link
        case "alert": .alert
        case "sheet": .sheet
        default: .any
        }
    }

    private func typeName(_ type: XCUIElement.ElementType) -> String {
        switch type {
        case .button: "button"
        case .staticText: "text"
        case .switch: "switch"
        case .toggle: "toggle"
        case .textField: "field"
        case .secureTextField: "secureField"
        case .textView: "textView"
        case .cell: "cell"
        case .image: "image"
        case .slider: "slider"
        case .alert: "alert"
        case .sheet: "sheet"
        case .navigationBar: "navBar"
        case .window: "window"
        default: "other"
        }
    }

    /// `label` matches exactly, `contains` matches a substring of the label, identifier or value.
    private func query(_ step: [String: Any]) -> XCUIElement {
        let a = app(step)
        var q = a.descendants(matching: elementType(step["type"] as? String))
        if let label = step["label"] as? String {
            q = q.matching(NSPredicate(format: "label == %@ OR identifier == %@", label, label))
        }
        if let part = step["contains"] as? String {
            q = q.matching(NSPredicate(format: "label CONTAINS %@ OR identifier CONTAINS %@ OR value CONTAINS %@", part, part, part))
        }
        let index = step["index"] as? Int ?? 0
        return q.element(boundBy: index)
    }

    /// The element's current snapshot; throws (instead of failing the test) when it is gone.
    private func snapshot(_ step: [String: Any], wait: Double) throws -> XCUIElementSnapshot {
        let e = query(step)
        guard e.waitForExistence(timeout: wait) else { throw StepError("not found: \(describe(step))") }
        do { return try e.snapshot() } catch { throw StepError("gone: \(describe(step))") }
    }

    /// Screen coordinate of a step: `point` [x, y] in points, or the centre of the matched
    /// element plus an optional `offset` [dx, dy].
    private func target(_ step: [String: Any], wait: Double, into result: inout [String: Any]) throws -> XCUICoordinate {
        let base = app(step).coordinate(withNormalizedOffset: .zero)
        if let point = step["point"] as? [Double] { return base.withOffset(CGVector(dx: point[0], dy: point[1])) }
        let snap = try snapshot(step, wait: wait)
        result["label"] = snap.label
        let offset = step["offset"] as? [Double] ?? [0, 0]
        return base.withOffset(CGVector(dx: snap.frame.midX + offset[0], dy: snap.frame.midY + offset[1]))
    }

    private func describe(_ step: [String: Any]) -> String {
        ["app", "type", "label", "contains", "index"].compactMap { k in step[k].map { "\(k)=\($0)" } }.joined(separator: " ")
    }
}
