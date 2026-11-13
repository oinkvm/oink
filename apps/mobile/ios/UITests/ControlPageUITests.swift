import XCTest

/// Control page system menus (focus list, lock / game menu) and keyboard taps on the design
/// fixtures (`-oink-ui-scene`). Screenshots go to `OINK_SHOT_DIR` when it is set
/// (`TEST_RUNNER_OINK_SHOT_DIR=… xcodebuild test`), otherwise they are only attached.
final class ControlPageUITests: XCTestCase {
    override func setUp() { continueAfterFailure = false }

    private func launch(_ scene: String, scheme: String) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", scene, "-oink-scheme", scheme, "-oink-theme", "pink"]
        app.launch()
        if scene.hasPrefix("control-keyboard") && XCUIDevice.shared.orientation.isLandscape {
            XCTAssertTrue(app.buttons["Esc"].waitForExistence(timeout: 15))
        } else {
            XCTAssertTrue(app.buttons["焦点设备：Office-PC"].waitForExistence(timeout: 15))
        }
        return app
    }

    private func shot(_ name: String) {
        let png = XCUIScreen.main.screenshot().pngRepresentation
        let attachment = XCTAttachment(data: png, uniformTypeIdentifier: "public.png")
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
        if let dir = ProcessInfo.processInfo.environment["OINK_SHOT_DIR"] {
            try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("\(name).png"))
        }
    }

    func testFocusMenu() {
        for scheme in ["light", "dark"] {
            let app = launch("control", scheme: scheme)
            app.buttons["焦点设备：Office-PC"].tap()
            XCTAssertTrue(app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "MacBook-Pro")).firstMatch.waitForExistence(timeout: 5))
            sleep(1)
            shot("control-focus-\(scheme)")
            app.terminate()
        }
    }

    func testLockMenuAndGameSubmenu() {
        for scheme in ["light", "dark"] {
            let app = launch("control", scheme: scheme)
            app.buttons["指针未锁定"].press(forDuration: 0.8)
            XCTAssertTrue(app.buttons["锁定指针"].waitForExistence(timeout: 5))
            sleep(1)
            shot("control-lock-menu-\(scheme)")
            app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "游戏模式")).firstMatch.tap()
            XCTAssertTrue(app.buttons["自动"].waitForExistence(timeout: 5))
            sleep(1)
            shot("control-game-menu-\(scheme)")
            app.terminate()
        }
    }

    /// 调节 menu (§2.1, 用户 2026-11-11): the 指针速度 slider is always there and follows touches;
    /// 单指划动 / 键盘布局 open a system menu over the still-open first menu, and picking an
    /// option closes only that menu and updates the row's value. (屏幕方向 is left alone: picking
    /// 横屏 would rotate the simulator.) The choices are stored, so each pass restores them.
    func testAdjustMenuSliderAndSecondLevels() {
        for scheme in ["light", "dark"] {
            let app = launch("control-adjust", scheme: scheme)
            let speed = app.otherElements["指针速度"]
            XCTAssertTrue(speed.waitForExistence(timeout: 5), "the slider shows without holding")
            let swipe = app.buttons["单指划动"]
            XCTAssertTrue(swipe.exists)

            // Slider: drag from a point on the track past the fast end, then past the slow end,
            // then touch just right of the middle. The points are fractions of the element, chosen
            // to land on the track whether its frame includes the row's padding or not.
            let at = { (x: CGFloat) in speed.coordinate(withNormalizedOffset: CGVector(dx: x, dy: 0.58)) }
            at(0.455).press(forDuration: 0.1, thenDragTo: at(1.5))
            XCTAssertEqual(speed.value as? String, "10")
            at(0.455).press(forDuration: 0.1, thenDragTo: at(-0.5))
            XCTAssertEqual(speed.value as? String, "1")
            at(0.52).tap()
            XCTAssertEqual(speed.value as? String, "6")
            sleep(1)
            shot("control-adjust-\(scheme)")

            // 单指划动: a system menu over the first menu.
            XCTAssertEqual(swipe.value as? String, "移动指针")
            swipe.tap()
            let scroll = app.buttons["滚动"]
            XCTAssertTrue(scroll.waitForExistence(timeout: 5))
            sleep(1)
            shot("control-swipe-\(scheme)")
            scroll.tap()
            XCTAssertTrue(scroll.waitForNonExistence(timeout: 5), "picking closes the second menu")
            XCTAssertTrue(speed.exists, "the first menu stays open")
            XCTAssertEqual(swipe.value as? String, "滚动")
            swipe.tap()
            let pointer = app.buttons["移动指针"]
            XCTAssertTrue(pointer.waitForExistence(timeout: 5))
            pointer.tap()
            XCTAssertTrue(pointer.waitForNonExistence(timeout: 5))
            XCTAssertEqual(swipe.value as? String, "移动指针")

            // 键盘布局: the long list, also over the first menu.
            let layout = app.buttons["键盘布局"]
            layout.tap()
            let jis = app.buttons["日语（JIS）"]
            XCTAssertTrue(jis.waitForExistence(timeout: 5))
            sleep(1)
            shot("control-layout-\(scheme)")
            jis.tap()
            XCTAssertTrue(jis.waitForNonExistence(timeout: 5))
            XCTAssertEqual(layout.value as? String, "日语（JIS）")
            layout.tap()
            let auto = app.buttons["自动"]
            XCTAssertTrue(auto.waitForExistence(timeout: 5))
            auto.tap()
            XCTAssertTrue(app.buttons["美式英语（US）"].waitForNonExistence(timeout: 5))
            XCTAssertEqual(layout.value as? String, "自动")
            XCTAssertTrue(speed.exists)
            app.terminate()
        }
    }

    /// Key preview bubbles (§3.1.4) on the design states: a finger rests on a key once the
    /// keyboard has opened and settled. `control-strip-*` sit on the system keyboard's accessory bar, where the top
    /// row's bubble reaches over the touchpad: the screenshots show whether it is clipped there.
    func testKeyPreviewStates() {
        let scenes = ["control-keyboard-preview", "control-keyboard-preview-edge", "control-keyboard-preview-space",
                      "control-keyboard-preview-multi", "control-strip-preview", "control-strip-preview-edge",
                      "control-strip-preview-wide", "control-strip-preview-arrow", "control-strip-preview-fkey"]
        for scheme in ["light", "dark"] {
            for scene in scenes {
                let app = launch(scene, scheme: scheme)
                XCTAssertTrue(app.buttons["Ctrl"].waitForExistence(timeout: 30))
                let intro = app.buttons["Continue"]
                if intro.waitForExistence(timeout: 1) { intro.tap() }
                sleep(5) // the scene waits for the keyboard to settle, then presses the key
                assertNoTabBar(app, scene)
                shot("\(scene)-\(scheme)")
                app.terminate()
            }
        }
    }

    /// Modifier three states and the temporary emulated keyboard on the real key caps.
    func testModifierTapsSwitchKeyboards() {
        let app = launch("control-keyboard-system", scheme: "light")
        let ctrl = app.buttons["Ctrl"]
        XCTAssertTrue(ctrl.waitForExistence(timeout: 30))
        // A fresh simulator covers its first system keyboard (and the bar above it) with the
        // slide-to-type introduction.
        let intro = app.buttons["Continue"]
        if intro.waitForExistence(timeout: 2) { intro.tap() }
        ctrl.tap()
        XCTAssertTrue(app.buttons["Ctrl（按住）"].waitForExistence(timeout: 3))
        XCTAssertTrue(app.buttons["符号"].waitForExistence(timeout: 3), "a held modifier brings the emulated keyboard")
        assertNoTabBar(app, "Ctrl tapped over the system keyboard")
        sleep(1)
        shot("control-keyboard-ctrl-tapped-light")
        app.buttons["Ctrl（按住）"].tap()
        XCTAssertTrue(app.buttons["Ctrl"].waitForExistence(timeout: 3))
        XCTAssertTrue(app.buttons["符号"].waitForNonExistence(timeout: 3), "all modifiers up: back to the system keyboard")
        app.buttons["Fn"].tap()
        XCTAssertTrue(app.buttons["F5"].waitForExistence(timeout: 3))
        app.buttons["F5"].tap()
        XCTAssertTrue(app.buttons["复制（Ctrl+C）"].waitForExistence(timeout: 3), "one F key, then the default row returns")
    }

    // MARK: 用户 2026-11-12 control page layout

    private let toSystem = "切换键盘：换到系统键盘（手机输入法）"
    private let toEmulated = "切换键盘：换到模拟键盘（发送原始键码）"

    override func tearDown() {
        XCUIDevice.shared.orientation = .portrait
        super.tearDown()
    }

    private func dismissKeyboardIntro(_ app: XCUIApplication) {
        let intro = app.buttons["Continue"]
        if intro.waitForExistence(timeout: 2) { intro.tap() }
    }

    /// Portrait: the keyboard opens from the button beside the tab bar; while it is up the top-right
    /// slot holds 切换键盘 and the control keys hold 收起键盘, which closes it again.
    func testPortraitKeyboardButtonSlotAndHideKey() {
        for scheme in ["light", "dark"] {
            let app = launch("control", scheme: scheme)
            XCTAssertFalse(app.buttons[toEmulated].exists, "the top-right slot is empty without a keyboard")
            XCTAssertFalse(app.buttons[toSystem].exists)
            sleep(1)
            shot("12-portrait-kb-hidden-\(scheme)")
            let open = app.buttons[Tab.keyboard]
            XCTAssertTrue(open.waitForExistence(timeout: 5), "keyboard button beside the tab bar")
            assertRestingTabBar(app)
            open.tap()
            let hide = app.buttons["收起键盘"]
            XCTAssertTrue(hide.waitForExistence(timeout: 10))
            dismissKeyboardIntro(app)
            XCTAssertTrue(app.buttons[toEmulated].waitForExistence(timeout: 5), "切换键盘 in the top-right (system keyboard)")
            XCTAssertTrue(app.buttons["焦点设备：Office-PC"].exists, "the header stays")
            assertNoTabBar(app, "system keyboard up")
            sleep(1)
            shot("12-portrait-kb-system-\(scheme)")
            app.buttons[toEmulated].tap()
            XCTAssertTrue(app.buttons["符号"].waitForExistence(timeout: 5), "the emulated keyboard")
            XCTAssertTrue(app.buttons[toSystem].exists)
            assertNoTabBar(app, "emulated keyboard up")
            sleep(1)
            shot("12-portrait-kb-emulated-\(scheme)")
            // Back to the remembered default for the next runs.
            app.buttons[toSystem].tap()
            hide.tap()
            XCTAssertTrue(hide.waitForNonExistence(timeout: 5), "收起键盘 closes the keyboard")
            XCTAssertTrue(open.waitForExistence(timeout: 5), "the tab bar and its keyboard button come back")
            assertRestingTabBar(app)
            app.terminate()
        }
    }

    /// The portrait tab bar at rest: the tabs 16 from the left (the first tab 4 inside the bar), the
    /// 62 keyboard button 16 from the right, both hittable (integration 400005: the bar stayed at
    /// the start of an entry animation, centred with a 37 pt invisible button).
    private func assertRestingTabBar(_ app: XCUIApplication, file: StaticString = #filePath, line: UInt = #line) {
        let open = app.buttons[Tab.keyboard]
        let devices = app.buttons[Tab.devices]
        guard open.waitForExistence(timeout: 5), devices.waitForExistence(timeout: 5) else {
            return failWithTree(app, "resting tab bar: keyboard button or 设备 missing", file: file, line: line)
        }
        let window = app.windows.firstMatch.frame
        var problems: [String] = []
        if !open.isHittable { problems.append("keyboard button not hittable") }
        if !devices.isHittable { problems.append("设备 not hittable") }
        if abs(open.frame.width - 62) > 1 { problems.append("keyboard button \(open.frame.width) wide, not 62") }
        let left = devices.frame.minX - 4
        if abs((left + open.frame.maxX) / 2 - window.midX) > 1 { problems.append("bar group is not centred") }
        if abs(open.frame.minX - (left + 274) - 8) > 1 { problems.append("gap is not 8") }
        if !problems.isEmpty { failWithTree(app, "resting tab bar: " + problems.joined(separator: "; "), file: file, line: line) }
    }

    /// While a keyboard is up neither the page's tab bar (with its keyboard button) nor the system
    /// one may be on screen (user 2026-11-12: the bar stayed over the keyboard on the device).
    private func assertNoTabBar(_ app: XCUIApplication, _ when: String, file: StaticString = #filePath, line: UInt = #line) {
        var problems: [String] = []
        if !app.buttons[Tab.keyboard].waitForNonExistence(timeout: 3) { problems.append("keyboard button still there") }
        for id in [Tab.devices, Tab.transfer, Tab.control] where app.buttons[id].exists {
            problems.append("\(id) still there")
        }
        let shownLabels = app.buttons.matching(NSPredicate(format: "label IN %@", ["设备", "传输"])).allElementsBoundByIndex
            .filter { $0.exists && $0.isHittable }
        if !shownLabels.isEmpty { problems.append("hittable tab: \(shownLabels.map(\.label))") }
        let bar = app.tabBars.firstMatch
        if bar.exists && bar.isHittable { problems.append("system tab bar hittable") }
        if !problems.isEmpty { failWithTree(app, "tab bar with the keyboard up (\(when)): " + problems.joined(separator: "; "), file: file, line: line) }
    }

    /// Fails with the whole element tree attached, so the failure explains itself.
    private func failWithTree(_ app: XCUIApplication, _ message: String, file: StaticString, line: UInt) {
        let tree = XCTAttachment(string: app.debugDescription)
        tree.name = "element tree: \(message)"
        tree.lifetime = .keepAlways
        add(tree)
        shot("FAIL-\(name)-\(Int(Date().timeIntervalSince1970))")
        XCTFail(message, file: file, line: line)
    }

    /// Identifiers of the page's own tabs and keyboard button (ControlTabID in the app).
    private enum Tab {
        static let keyboard = "control-tab-keyboard"
        static let devices = "control-tab-devices"
        static let transfer = "control-tab-transfer"
        static let control = "control-tab-control"
    }

    /// The 调节 button on a plain launch (the control-adjust scene opens the menu from the page's
    /// first state, which never exercised the button): it opens the menu, a tap outside closes it,
    /// and it still works after a keyboard has been up and down (user 2026-11-12: tapping 调节 did
    /// nothing on the device).
    func testAdjustButtonOpensMenu() {
        let app = launch("control", scheme: "light")
        let adjust = app.buttons["调节"]
        let speed = app.otherElements["指针速度"]
        XCTAssertTrue(adjust.waitForExistence(timeout: 5))
        XCTAssertFalse(speed.exists)
        adjust.tap()
        XCTAssertTrue(speed.waitForExistence(timeout: 3), "调节 opens the menu")
        XCTAssertTrue(app.buttons["键盘布局"].isHittable)
        // Outside the menu (lower part of the pad): closes it.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.75)).tap()
        XCTAssertTrue(speed.waitForNonExistence(timeout: 3), "a tap outside closes it")
        adjust.tap()
        XCTAssertTrue(speed.waitForExistence(timeout: 3), "opens again")
        adjust.tap()
        XCTAssertTrue(speed.waitForNonExistence(timeout: 3), "调节 again closes it")

        let open = app.buttons[Tab.keyboard]
        open.tap()
        let hide = app.buttons["收起键盘"]
        XCTAssertTrue(hide.waitForExistence(timeout: 10))
        dismissKeyboardIntro(app)
        hide.tap()
        XCTAssertTrue(open.waitForExistence(timeout: 5))
        adjust.tap()
        XCTAssertTrue(speed.waitForExistence(timeout: 3), "调节 works after the keyboard was up")
        app.terminate()
    }

    /// Rotating while the page is shown: the landscape top bar (tabs, keyboard toggle) and the 调节
    /// menu there, the keyboard from the toggle, then back to the portrait tab bar.
    func testRotateWhileShown() {
        let app = launch("control", scheme: "light")
        XCTAssertTrue(app.buttons[Tab.keyboard].waitForExistence(timeout: 5))
        XCUIDevice.shared.orientation = .landscapeLeft
        XCTAssertTrue(app.buttons[Tab.devices].waitForExistence(timeout: 5))
        let toggle = app.buttons["弹出键盘"]
        XCTAssertTrue(toggle.waitForExistence(timeout: 5), "keyboard toggle top-right")
        app.buttons["调节"].tap()
        XCTAssertTrue(app.otherElements["指针速度"].waitForExistence(timeout: 3), "调节 in landscape")
        app.buttons["调节"].tap()
        XCTAssertTrue(app.otherElements["指针速度"].waitForNonExistence(timeout: 3))
        toggle.tap()
        XCTAssertTrue(app.buttons["收起键盘"].waitForExistence(timeout: 10), "the toggle opens the keyboard in landscape")
        dismissKeyboardIntro(app)
        sleep(1)
        shot("12-landscape-rotated-kb-light")
        // The control keys' 收起键盘. The hidden floating bar's toggle (also 收起键盘 while the
        // keyboard is up) stays in XCUITest's tree, which lists views VoiceOver skips too; it takes
        // no touch (assertBarHidden).
        let close = app.buttons.matching(identifier: "收起键盘").allElementsBoundByIndex.filter(\.isHittable)
        XCTAssertEqual(close.count, 1, "one 收起键盘 to touch")
        close.first?.tap()
        XCTAssertTrue(app.buttons["弹出键盘"].waitForExistence(timeout: 5))
        XCUIDevice.shared.orientation = .portrait
        XCTAssertTrue(app.buttons[Tab.keyboard].waitForExistence(timeout: 5))
        assertRestingTabBar(app)
        app.terminate()
    }

    /// Item 2: the number row stays on both layers, `#+=` / `ABC` switch them, `,` and `.` sit beside
    /// the space bar on both, Shift shows the shifted digits.
    func testEmulatedKeyboardNumberRowAndLayers() {
        for scheme in ["light", "dark"] {
            let app = launch("control-keyboard", scheme: scheme)
            XCTAssertTrue(app.buttons["符号"].waitForExistence(timeout: 30))
            // The scene opens the keyboard right after the page's first frame (round 3: the tab bar
            // stayed drawn above the emulated keyboard).
            assertNoTabBar(app, "control-keyboard scene")
            for key in ["1", "0", "q", "a", "z", ",", "."] {
                XCTAssertTrue(app.buttons[key].exists, "letters layer has \(key)")
            }
            // A standard height (user 2026-11-12): the keys 266 ± 10 % (a standard system keyboard
            // seen before, else 266) + the accessory bar 134 above the home indicator (34 on the Face
            // ID iPhones); the control rows sit 7 inside the bar's top.
            let keyboardTop = app.buttons["Esc"].frame.minY - 7
            let keys = app.frame.maxY - keyboardTop - 134 - 34
            XCTAssertLessThanOrEqual(abs(keys - 266), 266 * 0.1 + 1, "the emulated keyboard's keys: \(keys)")
            sleep(1)
            shot("02-letters-\(scheme)")
            app.buttons["符号"].tap()
            XCTAssertTrue(app.buttons["字母"].waitForExistence(timeout: 3))
            for key in ["1", "0", "-", "[", "_", ",", "."] {
                XCTAssertTrue(app.buttons[key].exists, "symbol layer has \(key)")
            }
            sleep(1)
            shot("02-symbols-\(scheme)")
            app.terminate()

            let shifted = launch("control-keyboard-shift-held", scheme: scheme)
            XCTAssertTrue(shifted.buttons["!"].waitForExistence(timeout: 30), "Shift: 1 shows !")
            XCTAssertTrue(shifted.buttons["<"].exists, "Shift: , shows <")
            assertNoTabBar(shifted, "control-keyboard-shift-held scene")
            sleep(1)
            shot("02-shift-held-\(scheme)")
            shifted.terminate()
        }
    }

    /// Item 8: the gamepad shows only while game mode is on (auto included), without a tag.
    func testGameModeShowsOnlyWhileOn() {
        let idle = launch("control-game-auto-idle", scheme: "light")
        XCTAssertTrue(idle.buttons["指针未锁定"].waitForExistence(timeout: 5), "auto, not on: the lock shows")
        idle.terminate()
        for scheme in ["light", "dark"] {
            let on = launch("control-game-auto", scheme: scheme)
            XCTAssertTrue(on.buttons["游戏模式已开启（自动）"].waitForExistence(timeout: 5))
            sleep(1)
            shot("08-game-on-auto-\(scheme)")
            on.buttons["游戏模式已开启（自动）"].press(forDuration: 0.8)
            XCTAssertTrue(on.buttons["锁定指针"].waitForExistence(timeout: 5))
            sleep(1)
            shot("08-game-menu-\(scheme)")
            on.terminate()
        }
    }

    /// Landscape = the full screen (user 2026-11-12), both grips: the touchpad over the whole screen
    /// with the floating bar (tabs, keyboard button, 恢复); Oink's keyboard floating over the pad with
    /// 切换键盘 among its control keys and the bar hidden; the system keyboard with the control keys
    /// filling everything above it (切换键盘 ends the top row, 收起键盘 right of ↑) and no touchpad.
    ///
    func testLandscapeLayouts() {
        let grips: [(String, UIDeviceOrientation)] = [("grip-left", .landscapeLeft), ("grip-right", .landscapeRight)]
        for scheme in ["light", "dark"] {
            for (grip, orientation) in grips {
                XCUIDevice.shared.orientation = orientation
                let hidden = launch("control", scheme: scheme)
                XCTAssertTrue(hidden.buttons[Tab.devices].waitForExistence(timeout: 5), "tabs in the floating bar")
                XCTAssertTrue(hidden.buttons["弹出键盘"].exists, "keyboard button")
                XCTAssertTrue(hidden.buttons["恢复"].exists, "恢复 at the far right")
                XCTAssertTrue(hidden.otherElements["触摸板"].exists)
                sleep(2)
                shot("L-\(grip)-idle-\(scheme)")
                hidden.terminate()

                let emulated = launch("control-keyboard", scheme: scheme)
                XCTAssertTrue(emulated.buttons["符号"].waitForExistence(timeout: 30))
                XCTAssertTrue(emulated.buttons[toSystem].exists, "切换键盘 right of ↑ on the floating keyboard")
                XCTAssertTrue(emulated.buttons["Caps Lock"].exists, "Caps Lock instead of Shift in the control keys")
                XCTAssertTrue(emulated.otherElements["拖动键盘"].exists || emulated.descendants(matching: .any)["拖动键盘"].exists, "the handle")
                XCTAssertTrue(emulated.otherElements["触摸板"].exists, "the pad stays under the floating keyboard")
                assertBarHidden(emulated, "floating keyboard up")
                sleep(2)
                shot("F-\(grip)-float-default-\(scheme)")
                emulated.terminate()

                let system = launch("control-keyboard-system", scheme: scheme)
                XCTAssertTrue(system.buttons["Ctrl"].waitForExistence(timeout: 30))
                dismissKeyboardIntro(system)
                XCTAssertTrue(system.buttons[toEmulated].exists, "切换键盘 at the end of the top row")
                XCTAssertTrue(system.buttons["收起键盘"].exists, "收起键盘 right of ↑")
                XCTAssertTrue(system.otherElements["触摸板"].exists, "H >= 135: pad on the left")
                assertBarHidden(system, "system keyboard up")
                sleep(2)
                shot("L-\(grip)-kb-system-\(scheme)")
                system.terminate()
            }
        }
        XCUIDevice.shared.orientation = .portrait
    }

    /// The floating bar is gone (not hittable, out of the screen reader) while a keyboard is up.
    private func assertBarHidden(_ app: XCUIApplication, _ when: String, file: StaticString = #filePath, line: UInt = #line) {
        var problems: [String] = []
        for id in [Tab.devices, Tab.transfer, Tab.control] {
            let tab = app.buttons[id]
            if tab.exists && tab.isHittable { problems.append("\(id) hittable") }
        }
        let restore = app.buttons["恢复"]
        if restore.exists && restore.isHittable { problems.append("恢复 hittable") }
        if !problems.isEmpty { failWithTree(app, "floating bar with a keyboard up (\(when)): " + problems.joined(separator: "; "), file: file, line: line) }
    }

    /// 全屏 (portrait top-right, keyboard down) turns the interface to landscape with the phone
    /// upright; 恢复 turns it back. While the keyboard is up the top-right is 切换键盘 instead.
    func testFullscreenButtonAndRestore() {
        XCUIDevice.shared.orientation = .portrait
        let app = launch("control", scheme: "light")
        let full = app.buttons["全屏"]
        XCTAssertTrue(full.waitForExistence(timeout: 5), "全屏 top-right while no keyboard is up")
        sleep(1)
        shot("Z-portrait-fullscreen-button-light")
        full.tap()
        XCTAssertTrue(app.buttons["恢复"].waitForExistence(timeout: 5), "landscape full screen")
        XCTAssertTrue(app.buttons[Tab.devices].exists)
        sleep(2)
        shot("Z-fullscreen-by-button-light")
        app.buttons["恢复"].tap()
        XCTAssertTrue(app.buttons[Tab.keyboard].waitForExistence(timeout: 5), "portrait again")
        assertRestingTabBar(app)
        app.buttons[Tab.keyboard].tap()
        XCTAssertTrue(app.buttons["收起键盘"].waitForExistence(timeout: 10))
        dismissKeyboardIntro(app)
        XCTAssertFalse(app.buttons["全屏"].exists, "the top-right is 切换键盘 while a keyboard is up")
        XCTAssertTrue(app.buttons[toEmulated].exists)
        XCTAssertTrue(app.buttons["Caps Lock"].exists, "Caps Lock instead of Shift")
        app.buttons["收起键盘"].tap()
        app.terminate()
    }

    /// User 2026-11-12: the app only inactive (Control Center pulled down over it) keeps the
    /// full screen; going to the background ends it (the phone is upright, so portrait comes back).
    func testFullscreenSurvivesInactiveButNotBackground() {
        XCUIDevice.shared.orientation = .portrait
        let app = launch("control", scheme: "light")
        let full = app.buttons["全屏"]
        XCTAssertTrue(full.waitForExistence(timeout: 5))
        full.tap()
        let restore = app.buttons["恢复"]
        XCTAssertTrue(restore.waitForExistence(timeout: 5))

        // Control Center: a swipe down from the top-right corner; the app is only inactive under
        // it. (Not Notification Center: on iOS 26 it is the cover sheet, and once it is down UIKit
        // puts the app in the background (simulator log, integration 400006: deactivation reason
        // 11 and background tasks), which rightly leaves the page and ends the full screen.)
        let corner = app.coordinate(withNormalizedOffset: CGVector(dx: 0.97, dy: 0.0))
        corner.press(forDuration: 0.1, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.97, dy: 0.6)))
        sleep(2)
        // Back without a swipe from the bottom edge (the home gesture when nothing is over the
        // app): activating it closes Control Center.
        app.activate()
        XCTAssertEqual(app.state, .runningForeground)
        XCTAssertTrue(restore.waitForExistence(timeout: 5), "still the landscape full screen after the app was only inactive")
        shot("Z-fullscreen-after-inactive-light")

        // The background: leaving the page; with the phone upright the interface turns portrait.
        XCUIDevice.shared.press(.home)
        sleep(2)
        app.activate()
        XCTAssertTrue(app.buttons[Tab.keyboard].waitForExistence(timeout: 10), "portrait after the background")
        XCTAssertFalse(app.buttons["恢复"].exists)
        app.terminate()
    }

    /// 调节 (user 2026-11-12): 指针速度, three switches (键盘自动弹出 / 接收音频 / 共享麦克风), then
    /// 键盘布局 › and 单指划动 › — one column in portrait, two in landscape under the floating bar's
    /// 调节, which stays up while the menu is open.
    func testAdjustMenuSwitchesBothOrientations() {
        for (name, orientation) in [("portrait", UIDeviceOrientation.portrait), ("landscape", .landscapeLeft)] {
            XCUIDevice.shared.orientation = orientation
            for scheme in ["light", "dark"] {
                let app = launch("control", scheme: scheme)
                let adjust = app.buttons["调节"]
                XCTAssertTrue(adjust.waitForExistence(timeout: 5))
                adjust.tap()
                XCTAssertTrue(app.otherElements["指针速度"].waitForExistence(timeout: 3))
                for label in ["键盘自动弹出", "接收音频", "共享麦克风"] {
                    XCTAssertTrue(app.switches[label].exists, "\(label) switch (\(name))")
                }
                XCTAssertTrue(app.buttons["键盘布局"].isHittable)
                XCTAssertTrue(app.buttons["单指划动"].isHittable)
                XCTAssertFalse(app.buttons["屏幕方向"].exists, "屏幕方向 is gone (全屏 replaces it)")
                if name == "landscape" {
                    let speed = app.otherElements["指针速度"].frame
                    let layout = app.buttons["键盘布局"].frame
                    XCTAssertGreaterThan(layout.minX, speed.maxX - 1, "two columns: the choices right of the slider")
                }
                sleep(1)
                shot("\(name == "portrait" ? "P" : "Q")-adjust-\(scheme)")
                // 键盘自动弹出 is off by default (user 2026-11-12).
                XCTAssertEqual(app.switches["键盘自动弹出"].value as? String, "0")
                if name == "landscape" {
                    // While the menu is open its catcher covers the whole screen, the bar too (the
                    // first touch outside the menu only closes it), so the bar's buttons are not
                    // hittable then. The bar stayed up: right after that touch closes the menu its
                    // 调节 takes touches again, without the idle delay a hidden bar comes back after.
                    adjust.tap()
                    XCTAssertTrue(app.otherElements["指针速度"].waitForNonExistence(timeout: 3))
                    XCTAssertTrue(adjust.isHittable, "the bar stayed up while the menu was open")
                }
                app.terminate()
            }
        }
        XCUIDevice.shared.orientation = .portrait
    }

    /// The floating keyboard's handle (user 2026-11-12): sideways it moves the container; double-tap
    /// expands to the whole screen (laptop rows, two function rows with 收起 / 还原, PrtSc and the
    /// audio switches); 还原 brings the floating container back; down it dismisses the keyboard.
    func testFloatingKeyboardHandle() {
        XCUIDevice.shared.orientation = .landscapeLeft
        let app = launch("control-keyboard", scheme: "light")
        XCTAssertTrue(app.buttons["符号"].waitForExistence(timeout: 30))
        let handle = app.descendants(matching: .any)["拖动键盘"]
        XCTAssertTrue(handle.waitForExistence(timeout: 5))
        XCTAssertEqual(handle.frame.width, 420, accuracy: 1, "fixed reference-phone portrait width")
        XCTAssertGreaterThanOrEqual(handle.frame.height, 44)
        let esc = app.buttons["Esc"]
        let before = esc.frame.minX

        // Sideways: toward the other side.
        let start = handle.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        start.press(forDuration: 0.05, thenDragTo: start.withOffset(CGVector(dx: -300, dy: 0)))
        sleep(1)
        XCTAssertLessThan(esc.frame.minX, before - 100, "the container moved left")
        shot("F-float-moved-light")

        // Up no longer expands; double-tap expands to the whole screen.
        let top = handle.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        top.press(forDuration: 0.05, thenDragTo: top.withOffset(CGVector(dx: 0, dy: -120)))
        XCTAssertFalse(app.buttons["还原：回到浮动键盘"].exists, "upward drag must not expand")
        handle.doubleTap()
        let restore = app.buttons["还原：回到浮动键盘"]
        XCTAssertTrue(restore.waitForExistence(timeout: 5), "expanded by double tap")
        assertBarHidden(app, "expanded")
        for label in ["Print Screen", "接收音频", "共享麦克风", "Caps Lock", "收起键盘", "Tab"] {
            XCTAssertTrue(app.buttons[label].exists, "\(label) on the expanded keyboard")
        }
        sleep(1)
        shot("E-expanded-light")
        restore.tap()
        XCTAssertTrue(handle.waitForExistence(timeout: 5), "floating again")
        XCTAssertTrue(app.buttons["符号"].waitForExistence(timeout: 3))

        // Down: dismissed.
        let down = handle.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        down.press(forDuration: 0.05, thenDragTo: down.withOffset(CGVector(dx: 0, dy: 200)))
        XCTAssertTrue(app.buttons["符号"].waitForNonExistence(timeout: 5), "dismissed")
        XCTAssertTrue(app.buttons["弹出键盘"].waitForExistence(timeout: 5), "the bar comes back")
        app.terminate()
        XCUIDevice.shared.orientation = .portrait
    }

    /// Coordinate taps exercise the physical right edge, rather than an accessibility action.
    func testLandscapeRightEdgeTaps() {
        XCUIDevice.shared.orientation = .landscapeLeft
        defer { XCUIDevice.shared.orientation = .portrait }
        var app = launch("control", scheme: "light")
        let pad = app.otherElements["触摸板"]
        XCTAssertTrue(pad.waitForExistence(timeout: 5))
        let edge = app.coordinate(withNormalizedOffset: CGVector(dx: 1, dy: 0.5)).withOffset(CGVector(dx: -10, dy: 0))
        edge.press(forDuration: 0.2, thenDragTo: edge.withOffset(CGVector(dx: -15, dy: 0)))
        XCTAssertEqual(pad.value as? String, "1", "right-edge touch reached the UIKit pad")
        // The bar returns with the independent 0.4 s delay.
        XCTAssertTrue(app.buttons["弹出键盘"].waitForExistence(timeout: 2))
        app.terminate()
        app = launch("control-keyboard", scheme: "light")
        dismissKeyboardIntro(app)
        let handle = app.descendants(matching: .any)["拖动键盘"]
        XCTAssertTrue(handle.waitForExistence(timeout: 5))
        // The switch occupies the rightmost control column in the formerly dead strip.
        let switchKey = app.buttons[toSystem]
        XCTAssertTrue(switchKey.waitForExistence(timeout: 5))
        XCTAssertGreaterThan(switchKey.frame.midX, app.frame.maxX - 120)
        XCTAssertLessThanOrEqual(switchKey.frame.maxX, app.frame.maxX - 59 + 1)
        switchKey.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
        XCTAssertTrue(handle.waitForNonExistence(timeout: 5), "right-edge key switched to system keyboard")
        XCTAssertTrue(app.buttons[toEmulated].waitForExistence(timeout: 5))
        assertBarHidden(app, "system keyboard after right-edge tap")
        shot("right-edge-key-tap")
        app.terminate()
    }

    /// Top-strip touches belong to the enlarged keyboard handle, never the pad beneath it.
    func testHandleCapturesStripAboveKeyboard() {
        let initialOrientation = XCUIDevice.shared.orientation
        defer { XCUIDevice.shared.orientation = initialOrientation }
        XCUIDevice.shared.orientation = .landscapeLeft
        let app = launch("control-keyboard", scheme: "light")
        let handle = app.descendants(matching: .any)["拖动键盘"]
        XCTAssertTrue(handle.waitForExistence(timeout: 5))
        let pad = app.otherElements["触摸板"]
        let before = pad.value as? String
        let top = app.coordinate(withNormalizedOffset: .zero).withOffset(CGVector(dx: handle.frame.midX, dy: 6))
        top.press(forDuration: 0.05, thenDragTo: top.withOffset(CGVector(dx: -100, dy: 0)))
        XCTAssertEqual(pad.value as? String, before, "top strip must not send pad input")
        XCTAssertTrue(app.buttons["符号"].exists)
        shot("handle-top-strip")
        app.terminate()
    }

    /// A pad finger crossing a visible top-bar button continues its pad operation.
    func testPadGestureCrossesVisibleTopBar() {
        let initialOrientation = XCUIDevice.shared.orientation
        defer { XCUIDevice.shared.orientation = initialOrientation }
        XCUIDevice.shared.orientation = .landscapeLeft
        let app = launch("control", scheme: "light")
        let adjust = app.buttons["调节"]
        let start = app.otherElements["触摸板"].coordinate(withNormalizedOffset: CGVector(dx: 0.15, dy: 0.7))
        start.press(forDuration: 0.05, thenDragTo: adjust.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)))
        XCTAssertTrue(adjust.exists && adjust.isHittable, "bar remains visible and returns input immediately")
        XCTAssertFalse(app.otherElements["指针速度"].exists)
        adjust.tap()
        XCTAssertTrue(app.otherElements["指针速度"].waitForExistence(timeout: 3), "a fresh bar touch still opens it")
        app.terminate()
    }

    func testTopBarGapRoutesToPad() {
        let initialOrientation = XCUIDevice.shared.orientation
        defer { XCUIDevice.shared.orientation = initialOrientation }
        XCUIDevice.shared.orientation = .landscapeLeft
        let app = launch("control", scheme: "light")
        let keyboard = app.buttons["弹出键盘"]
        let restore = app.buttons["恢复"]
        XCTAssertTrue(keyboard.waitForExistence(timeout: 5))
        let pad = app.otherElements["触摸板"]
        let gap = CGPoint(x: (keyboard.frame.maxX + restore.frame.minX) / 2, y: keyboard.frame.midY)
        app.coordinate(withNormalizedOffset: .zero).withOffset(CGVector(dx: gap.x, dy: gap.y)).tap()
        XCTAssertEqual(pad.value as? String, "1", "bar gaps belong to the pad even while idle")
        XCTAssertTrue(keyboard.exists)
        app.terminate()
    }

    func testPortraitBarTransitionRoundTrip() {
        let initialOrientation = XCUIDevice.shared.orientation
        defer { XCUIDevice.shared.orientation = initialOrientation }
        XCUIDevice.shared.orientation = .portrait
        let app = launch("control", scheme: "light")
        assertRestingTabBar(app)
        app.buttons[Tab.devices].tap()
        let control = app.tabBars.buttons["控制"]
        XCTAssertTrue(control.waitForExistence(timeout: 5))
        control.tap()
        XCTAssertTrue(app.buttons[Tab.keyboard].waitForExistence(timeout: 5))
        // Wait on the final frame via the normal UI accessibility snapshot, not an animation timer.
        assertRestingTabBar(app)
        app.terminate()
    }

    /// Same fixtures on simulator and phone, without touching system appearance preferences.
    func testR3Screenshots() {
        let initialOrientation = XCUIDevice.shared.orientation
        defer { XCUIDevice.shared.orientation = initialOrientation }
        for scheme in ["light", "dark"] {
            XCUIDevice.shared.orientation = .portrait
            let portrait = launch("control", scheme: scheme)
            assertRestingTabBar(portrait)
            print("R3 reference portrait points: \(portrait.frame.size)")
            shot("r3-portrait-\(scheme)")
            portrait.terminate()
            XCUIDevice.shared.orientation = .landscapeLeft
            let landscape = launch("control", scheme: scheme)
            shot("r3-landscape-idle-\(scheme)")
            landscape.terminate()
            let keys = launch("control-keyboard", scheme: scheme)
            let handle = keys.descendants(matching: .any)["拖动键盘"]
            XCTAssertTrue(handle.waitForExistence(timeout: 5))
            XCTAssertEqual(handle.frame.width, 420, accuracy: 1)
            XCTAssertGreaterThanOrEqual(handle.frame.height, 44)
            print("R3 floating handle points: \(handle.frame)")
            shot("r3-landscape-keyboard-\(scheme)")
            keys.terminate()
        }
    }

    /// Uses the installed system dictation entry, never accepts an authorization prompt. No
    /// text is injected; any speech/permission interaction must be completed by the device owner.
    func testDictationPromptKeepsKeyboard() throws {
        let initialOrientation = XCUIDevice.shared.orientation
        defer { XCUIDevice.shared.orientation = initialOrientation }
        XCUIDevice.shared.orientation = .landscapeLeft
        let app = XCUIApplication()
        app.launch()
        defer { app.terminate() }
        if app.tabBars.buttons["控制"].waitForExistence(timeout: 2) { app.tabBars.buttons["控制"].tap() }
        if app.buttons["全屏"].waitForExistence(timeout: 1) { app.buttons["全屏"].tap() }
        let open = app.buttons["弹出键盘"]
        XCTAssertTrue(open.waitForExistence(timeout: 10))
        open.tap()
        let wasEmulated = app.buttons[toSystem].waitForExistence(timeout: 2)
        if wasEmulated { app.buttons[toSystem].tap() }
        defer {
            // Terminating dismisses an unaccepted system prompt without changing its setting.
            // Restore Oink's keyboard choice through the UI if this probe changed it.
            app.terminate()
            if wasEmulated {
                app.launch()
                if app.tabBars.buttons["控制"].waitForExistence(timeout: 2) { app.tabBars.buttons["控制"].tap() }
                if app.buttons["全屏"].waitForExistence(timeout: 1) { app.buttons["全屏"].tap() }
                if open.waitForExistence(timeout: 5) { open.tap() }
                if app.buttons[toEmulated].waitForExistence(timeout: 3) { app.buttons[toEmulated].tap() }
                if app.buttons["收起键盘"].isHittable { app.buttons["收起键盘"].tap() }
            }
        }
        let dictate = app.buttons.matching(NSPredicate(format: "label CONTAINS[c] 'dictat' OR label CONTAINS '听写' OR label CONTAINS '语音输入'")).firstMatch
        guard dictate.waitForExistence(timeout: 5) else {
            shot("dictation-entry-unavailable")
            let tree = XCTAttachment(string: app.debugDescription); tree.name = "dictation-entry-tree"; tree.lifetime = .keepAlways; add(tree)
            throw XCTSkip("当前系统键盘未提供可自动定位的听写入口")
        }
        shot("dictation-before")
        dictate.tap()
        sleep(3)
        shot("dictation-after")
        let tree = XCTAttachment(string: app.debugDescription)
        tree.name = "dictation-after-tree"; tree.lifetime = .keepAlways; add(tree)
        XCTAssertFalse(open.exists, "system prompt must not close the control keyboard session")
        print("R3 dictation system prompt count: \(app.alerts.count)")
    }

    /// Synthetic H boundaries exercise both branches independently of installed IME heights.
    func testLandscapeSystemKeyboardThreshold() {
        XCUIDevice.shared.orientation = .landscapeLeft
        defer { XCUIDevice.shared.orientation = .portrait }
        for height in [134, 135] {
            let app = launch("control-keyboard-system-h\(height)", scheme: "light")
            dismissKeyboardIntro(app)
            assertBarHidden(app, "H=\(height)")
            XCTAssertEqual(app.otherElements["触摸板"].exists, height >= 135)
            let esc = app.buttons["Esc"].frame
            let caps = app.buttons["Caps Lock"].frame
            let ctrl = app.buttons["Ctrl"].frame
            let home = app.buttons["Home"].frame
            if height < 135 {
                XCTAssertEqual(home.midY, esc.midY, accuracy: 1)
                XCTAssertLessThan(home.minX, app.buttons["快退 / 上一首"].frame.minX)
                XCTAssertEqual(ctrl.midY, caps.midY, accuracy: 1)
            } else {
                XCTAssertGreaterThan(esc.minX, app.frame.width / 2 - 1)
                XCTAssertGreaterThan(caps.minY, esc.minY)
                XCTAssertGreaterThan(ctrl.minY, caps.minY)
                XCTAssertEqual(app.buttons["上"].frame.midX, app.buttons["下"].frame.midX, accuracy: 1)
            }
            shot("system-h\(height)-normal")
            app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Fn")).firstMatch.tap()
            for n in 1...12 { XCTAssertTrue(app.buttons["F\(n)"].waitForExistence(timeout: 3)) }
            XCTAssertTrue(app.buttons["Esc"].exists)
            XCTAssertTrue(app.buttons[toEmulated].exists)
            XCTAssertFalse(app.buttons["Home"].exists && height == 134)
            shot("system-h\(height)-fn")
            app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Fn")).firstMatch.tap()
            XCTAssertTrue(app.buttons["Home"].waitForExistence(timeout: 3))
            app.buttons["收起键盘"].tap()
            XCTAssertTrue(app.buttons["弹出键盘"].waitForExistence(timeout: 3))
            app.terminate()
        }
    }

    /// The expanded keyboard as a design state.
    func testExpandedKeyboardScene() {
        XCUIDevice.shared.orientation = .landscapeLeft
        for scheme in ["light", "dark"] {
            let app = launch("control-keyboard-expanded", scheme: scheme)
            XCTAssertTrue(app.buttons["还原：回到浮动键盘"].waitForExistence(timeout: 30))
            XCTAssertTrue(app.buttons["`"].exists && app.buttons["\\"].exists, "laptop rows")
            sleep(1)
            shot("E-expanded-\(scheme)")
            app.terminate()
        }
        let shifted = launch("control-keyboard-expanded-shift-held", scheme: "light")
        XCTAssertTrue(shifted.buttons["~"].waitForExistence(timeout: 30), "Shift: ` shows ~")
        XCTAssertTrue(shifted.buttons["!"].exists)
        sleep(1)
        shot("E2-expanded-shift-light")
        shifted.terminate()
        XCUIDevice.shared.orientation = .portrait
    }
}
