import UIKit
import XCTest
@testable import OinkAudioTestHost

@MainActor
private final class Recorder: RemoteInputSink {
    var out: [String] = []
    func fieldText(_ text: String) { out.append("text \(text)") }
    func fieldBackspace() { out.append("backspace") }
}

private extension RemoteInputField {
    func range(_ a: Int, _ b: Int) -> UITextRange {
        textRange(from: position(from: beginningOfDocument, offset: a)!, to: position(from: beginningOfDocument, offset: b)!)!
    }
    func mark(_ text: String) { setMarkedText(text, selectedRange: NSRange(location: (text as NSString).length, length: 0)) }
}

/// The system keyboard's hidden field (control-page.md §3.1.1, user 2026-11-12): what reaches the
/// focus computer for scripted input-method call sequences, the same sequences as Android's
/// RemoteInputViewTest. Marked text never goes out and a commit goes out once; the committed text
/// stays in the field, so an input method that rewrites text it committed earlier finds it there,
/// and the rewrite reaches the computer as Backspaces from the end plus the text typed again.
/// `batch` is one input-method event: the field is read once it is over (`settleNow`).
@MainActor
final class RemoteInputFieldTests: XCTestCase {
    private var window: UIWindow?

    private func make() -> (RemoteInputField, Recorder) {
        let sink = Recorder()
        let field = RemoteInputField()
        field.sink = sink
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(field)
        self.window = window
        return (field, sink)
    }

    private func batch(_ field: RemoteInputField, _ body: () -> Void) {
        body()
        field.settleNow()
    }

    private func backspaces(_ n: Int) -> [String] { Array(repeating: "backspace", count: n) }

    func testPinyinMarkedTextIsNotSentAndTheCommitGoesOnce() {
        let (f, sink) = make()
        for s in ["n", "ni", "nih", "nihao"] { batch(f) { f.mark(s) } }
        XCTAssertEqual(sink.out, [])
        // A candidate replacing the marked text…
        batch(f) { f.insertText("你好") }
        XCTAssertEqual(sink.out, ["text 你好"])
        // …or the marked text committed as it is.
        batch(f) { f.mark("ma") }
        batch(f) { f.mark("吗"); f.unmarkText() }
        XCTAssertEqual(sink.out, ["text 你好", "text 吗"])
        XCTAssertEqual(f.text, "你好吗", "the committed text stays in the field")
    }

    /// 九键: a candidate for part of the pinyin is committed in front of the rest, the next one takes
    /// the committed character back into the composition with the rest, and the last commit
    /// carries both — all inside single events. The character goes out once.
    func testNineKeyPartialCandidatesAreSentOnce() {
        let (f, sink) = make()
        batch(f) { f.mark("nihao") }
        batch(f) { f.mark("你"); f.unmarkText(); f.mark("hao") }
        XCTAssertEqual(sink.out, ["text 你"])
        batch(f) {
            f.unmarkText()
            f.selectedRange = NSRange(location: 0, length: 4)
            f.mark("你hao")
        }
        XCTAssertEqual(sink.out, ["text 你"], "taken back into the composition: waits")
        batch(f) { f.insertText("你好") }
        XCTAssertEqual(sink.out, ["text 你", "text 好"])
    }

    /// 豆包 / 讯飞-style voice input with 识别优化 and system dictation (user 2026-11-12): each
    /// sentence is recognised as marked text and committed, then rewritten — the second time
    /// together with the end of the first sentence. The caret stays at the end, so each sentence
    /// goes after the last one, and a rewrite of text already on the computer is Backspace from
    /// the end back to the first changed character, then the rest typed again.
    func testVoiceSentencesAndTheirRewritesGoOutAtTheEnd() {
        let (f, sink) = make()
        batch(f) { f.mark("今天") }
        batch(f) { f.mark("今天天气") }
        XCTAssertEqual(sink.out, [])
        batch(f) { f.insertText("今天天气很好") }
        XCTAssertEqual(sink.out, ["text 今天天气很好"])
        // Rewrite of sentence 1 (a full stop added) over the committed text.
        batch(f) { f.replace(f.range(0, 6), withText: "今天天气很好。") }
        XCTAssertEqual(sink.out, ["text 今天天气很好", "text 。"])
        XCTAssertEqual(f.text, "今天天气很好。")
        XCTAssertEqual(f.selectedRange, NSRange(location: 7, length: 0), "caret at the end")

        // Sentence 2 streams at the caret; nothing goes out until it is committed.
        batch(f) { f.mark("我们") }
        batch(f) { f.mark("我们去公园") }
        XCTAssertEqual(sink.out.count, 2)
        batch(f) { f.insertText("我们去公园吧") }
        XCTAssertEqual(sink.out.last, "text 我们去公园吧")
        sink.out.removeAll()

        // The rewrite spans both sentences: "很好。我们去公园吧" → "不错，我们去公园吧。".
        batch(f) { f.replace(f.range(4, 13), withText: "不错，我们去公园吧。") }
        XCTAssertEqual(sink.out, backspaces(9) + ["text 不错，我们去公园吧。"])
        XCTAssertEqual(f.text, "今天天气不错，我们去公园吧。")
        XCTAssertEqual(f.selectedRange, NSRange(location: 14, length: 0), "caret stays at the end")
        sink.out.removeAll()

        // Sentence 3 goes after it.
        batch(f) { f.mark("明天") }
        batch(f) { f.insertText("明天见") }
        XCTAssertEqual(sink.out, ["text 明天见"])
    }

    /// Rewrites by selection + insert, by deletion + insert, and by marking committed text again.
    func testRewritesOfSentTextAreBackspacedFromTheEndAndRetyped() {
        let (f, sink) = make()
        batch(f) { f.insertText("abc def") }
        batch(f) { f.selectedRange = NSRange(location: 4, length: 3); f.insertText("xyz.") }
        XCTAssertEqual(sink.out, ["text abc def"] + backspaces(3) + ["text xyz."])
        sink.out.removeAll()
        batch(f) { for _ in 0..<4 { f.deleteBackward() }; f.insertText("XYZ!") }
        XCTAssertEqual(sink.out, backspaces(4) + ["text XYZ!"])
        sink.out.removeAll()
        // Taken back into the composition: waits until the input method is done with it.
        batch(f) { f.selectedRange = NSRange(location: 0, length: 3); f.mark("ABC") }
        XCTAssertEqual(sink.out, [])
        batch(f) { f.unmarkText() }
        XCTAssertEqual(sink.out, backspaces(8) + ["text ABC XYZ!"])
    }

    func testMirrorCostIsEverythingFromTheFirstChangedCharacter() {
        XCTAssertEqual(ImeMirror.step(sent: "abc def", field: "abX def!", marked: nil),
                       MirrorStep(backspaces: 5, text: "X def!", sent: "abX def!"))
        XCTAssertEqual(ImeMirror.step(sent: "a👍🏽", field: "a👍🏿", marked: nil),
                       MirrorStep(backspaces: 1, text: "👍🏿", sent: "a👍🏿"))
        XCTAssertEqual(ImeMirror.step(sent: "abc", field: "abXc", marked: NSRange(location: 2, length: 1)),
                       MirrorStep(backspaces: 0, text: "", sent: "abc"))
        XCTAssertEqual(ImeMirror.step(sent: "你", field: "你好hao", marked: NSRange(location: 2, length: 3)),
                       MirrorStep(backspaces: 0, text: "好", sent: "你好"))
    }

    func testMarkedTextLeftWhenTheKeyboardHidesIsNotSent() {
        let (f, sink) = make()
        batch(f) { f.mark("ni") }
        f.textViewDidEndEditing(f)
        RunLoop.main.run(until: Date().addingTimeInterval(0.05))
        XCTAssertEqual(sink.out, [])
        XCTAssertEqual(f.text, "")
        XCTAssertNil(f.markedTextRange)
    }

    func testBackspaceOnTheEmptyFieldAndWhenHeld() {
        let (f, sink) = make()
        // A held delete key calls deleteBackward once per repeat.
        for _ in 0..<3 { f.deleteBackward() }
        XCTAssertEqual(sink.out, backspaces(3))
    }

    func testBackspaceOverSentTextDeletesItOnTheComputer() {
        let (f, sink) = make()
        batch(f) { f.insertText("ab") }
        batch(f) { f.deleteBackward() }
        batch(f) { f.deleteBackward() }
        f.deleteBackward()                                                              // now empty
        XCTAssertEqual(sink.out, ["text ab"] + backspaces(3))
    }

    /// The computer's caret is behind the field's text, so Backspace in front of it deletes nothing there either.
    func testBackspaceAtTheStartOfTextInTheFieldStaysInTheField() {
        let (f, sink) = make()
        batch(f) { f.insertText("ab") }
        batch(f) { f.selectedRange = NSRange(location: 0, length: 0); f.deleteBackward() }
        XCTAssertEqual(sink.out, ["text ab"])
        XCTAssertEqual(f.text, "ab")
    }

    func testDeletingAnEmojiIsOneBackspace() {
        let (f, sink) = make()
        batch(f) { f.insertText("a👍🏽") }
        batch(f) { f.deleteBackward() }
        XCTAssertEqual(sink.out, ["text a👍🏽", "backspace"])
    }

    /// Return is a line break in the field (the Return key on the computer, ControlModel), so a
    /// later Backspace or rewrite counts it.
    func testReturnLivesInTheField() {
        let (f, sink) = make()
        batch(f) { f.insertText("ok"); f.insertText("\n") }
        batch(f) { f.insertText("7") }
        batch(f) { f.deleteBackward(); f.deleteBackward() }                            // "7" and the line break
        batch(f) { f.insertText("\n") }
        XCTAssertEqual(sink.out, ["text ok\n", "text 7", "backspace", "backspace", "text \n"])
        XCTAssertEqual(f.text, "ok\n")
    }

    /// The page moved the computer's caret (its own keys, a click, another field): marked text is
    /// dropped and the field is empty.
    func testRestartEmptiesTheField() {
        let (f, sink) = make()
        batch(f) { f.insertText("ab") }
        batch(f) { f.mark("ni") }
        f.restart("test")
        XCTAssertEqual(f.text, "")
        XCTAssertNil(f.markedTextRange)
        XCTAssertEqual(f.sent, "")
        batch(f) { f.insertText("你") }
        XCTAssertEqual(sink.out, ["text ab", "text 你"])
    }

    func testALongFieldStartsAfresh() {
        let (f, sink) = make()
        let long = String(repeating: "a", count: RemoteInputField.maxField + 1)
        batch(f) { f.insertText(long) }
        XCTAssertEqual(sink.out, ["text \(long)"])
        XCTAssertEqual(f.text, "")
        batch(f) { f.insertText("b") }
        XCTAssertEqual(sink.out.last, "text b")
        XCTAssertEqual(sink.out.count, 2, "no Backspace for the text the field let go of")
    }

    /// What restarts the field (Android's ControlFieldRestartTest): keys that may type, delete or
    /// move the caret; not modifiers, Caps Lock or media keys; another text field or computer.
    func testRestartRules() {
        for code in [HID.leftShift, HID.leftControl, HID.key(0xE7), HID.capsLock, HID.volumeUp, HID.playPause, HID.mute] {
            XCTAssertFalse(FieldRestartRules.restarts(keyDown: code), String(code, radix: 16))
        }
        for code in [HID.left, HID.key(0x04), HID.returnKey, HID.backspace, HID.escape, HID.v, HID.function(5)] {
            XCTAssertTrue(FieldRestartRules.restarts(keyDown: code), String(code, radix: 16))
        }
        var rules = FieldRestartRules()
        let pc = String(repeating: "a", count: 64)
        XCTAssertTrue(rules.textFocus(PCTextFocusReport(device: pc, seq: 1, editable: true, secure: false)))
        XCTAssertFalse(rules.textFocus(PCTextFocusReport(device: pc, seq: 1, editable: true, secure: false)), "the same field again")
        XCTAssertTrue(rules.textFocus(PCTextFocusReport(device: pc, seq: 2, editable: true, secure: false)))
        XCTAssertTrue(rules.focusDevice(pc))
        XCTAssertFalse(rules.focusDevice(pc), "the same computer")
        XCTAssertTrue(rules.focusDevice(String(repeating: "b", count: 64)))
        XCTAssertTrue(rules.focusDevice(nil))
    }

    /// The diagnostics name the calls with lengths and offsets, never the text.
    func testDiagnosticsCarryNoText() {
        var lines: [String] = []
        ImeLog.capture = { lines.append($0) }
        defer { ImeLog.capture = nil }
        let (f, _) = make()
        batch(f) { f.mark("mima") }
        batch(f) { f.insertText("秘密") }
        XCTAssertTrue(lines.contains { $0.hasPrefix("setMarkedText n=4") }, lines.joined(separator: "\n"))
        XCTAssertTrue(lines.contains { $0.hasPrefix("insertText n=2") }, lines.joined(separator: "\n"))
        XCTAssertTrue(lines.contains { $0.hasPrefix("out backspaces=0 text=2") }, lines.joined(separator: "\n"))
        XCTAssertFalse(lines.contains { $0.contains("秘") || $0.contains("mima") })
    }
}
