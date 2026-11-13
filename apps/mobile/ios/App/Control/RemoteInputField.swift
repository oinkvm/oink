import UIKit
import os

/// What the hidden field hands over to the focus computer.
@MainActor
protocol RemoteInputSink: AnyObject {
    /// Committed text; a line break or tab in it is the Return / Tab key (`TextPiece`).
    func fieldText(_ text: String)
    /// One Backspace (a deleted committed character, or Backspace on the empty field).
    func fieldBackspace()
}

/// What to send so the focus computer shows the field's committed text again: `backspaces`
/// characters off the end of what it has, then `text`; `sent` is what it then has.
struct MirrorStep: Equatable {
    var backspaces: Int
    var text: String
    var sent: String
}

/// The field's committed text as the focus computer has it (control-page.md §3.1.1, user
/// 2026-11-12; the same rules as Android's `ImeMirror`). Only text outside the marked range
/// counts. While the input method has marked text, text that only grows the committed part is
/// sent (a candidate committed in front of the rest of the pinyin); anything else waits until
/// the marking ends, because the input method has taken already sent text back into its
/// composition and normally commits it again unchanged. Without marked text, the difference to
/// what was sent goes out: Backspace for each character (grapheme cluster, as Backspace deletes
/// them) that is gone from the end of the common part, then the new text.
///
/// The computer's caret is at the end of what was sent: the field only ever types there, and
/// anything that moves the caret on the computer restarts the field (`RemoteInputField.restart`).
/// So an edit that is not at the end — a voice input method rewriting the last sentences after
/// recognition, a dictation correction, a candidate replacing a word further back — costs a
/// Backspace for every character from the first changed one to the end, and the rest typed again
/// from there. Nothing assumes where the caret is beyond that (no arrow keys: how far one moves
/// differs between apps).
enum ImeMirror {
    /// `marked`: the marked range in UTF-16 offsets (UITextInput's), nil without marked text.
    static func step(sent: String, field: String, marked: NSRange?) -> MirrorStep {
        if let marked, marked.length > 0 {
            let start = min(marked.location, (field as NSString).length)
            let committed = (field as NSString).substring(to: start)
            return committed.hasPrefix(sent)
                ? MirrorStep(backspaces: 0, text: String(committed.dropFirst(sent.count)), sent: committed)
                : MirrorStep(backspaces: 0, text: "", sent: sent)
        }
        if field == sent { return MirrorStep(backspaces: 0, text: "", sent: sent) }
        let old = Array(sent), new = Array(field)
        var k = 0
        while k < min(old.count, new.count) && old[k] == new[k] { k += 1 }
        return MirrorStep(backspaces: old.count - k, text: String(new[k...]), sent: field)
    }
}

/// When the hidden field must start afresh because something other than the field typed at the
/// focus computer's caret or moved it (the same rules as Android's ControlHolder): a key from the
/// page or a hardware keyboard (not the field's own Backspace / Return, not modifiers, Caps Lock
/// or media keys), a touchpad click, another text field there, another focus computer.
struct FieldRestartRules {
    private var focus: PCTextFocusReport?
    private var device: String?

    /// Keys that leave the computer's text and caret as they are: modifiers, Caps Lock and the
    /// media (consumer page) keys. Any other key may type, delete or move the caret there.
    static func keepsCaret(_ code: UInt32) -> Bool {
        code >> 16 != 0x07 || (0xE0...0xE7).contains(code & 0xFFFF) || code == HID.capsLock
    }

    /// A key went down on the computer, not from the field.
    static func restarts(keyDown code: UInt32) -> Bool { !keepsCaret(code) }

    /// The computer's text focus report: a different one (another field, another focus episode).
    mutating func textFocus(_ report: PCTextFocusReport) -> Bool {
        guard report != focus else { return false }
        focus = report
        return true
    }

    /// The focus computer, after each state change: a different one.
    mutating func focusDevice(_ value: String?) -> Bool {
        guard value != device else { return false }
        device = value
        return true
    }
}

/// Diagnostics of what the input method does with the field (category OinkIme, user 2026-11-12,
/// as Android's OinkIme tag): one line per input call and per edit sent to the computer, with
/// lengths and offsets only, never the text. At most `perSecond` lines a second; a line says how
/// many were left out.
@MainActor
enum ImeLog {
    private static let logger = Logger(subsystem: "com.hachiwii.oink", category: "OinkIme")
    static let perSecond = 100
    private static var window = -Double.infinity
    private static var lines = 0
    private static var dropped = 0
    /// Tests: every line, before the rate limit.
    static var capture: ((String) -> Void)?

    static func line(_ message: () -> String) {
        let now = ProcessInfo.processInfo.systemUptime
        if now - window >= 1 {
            let left = dropped
            if left > 0 { logger.info("(\(left) lines left out)") }
            window = now; lines = 0; dropped = 0
        }
        let tap = Self.capture
        if lines >= perSecond && tap == nil { dropped += 1; return }
        lines += 1
        let text = message()
        tap?(text)
        logger.info("\(text, privacy: .public)")
    }
}

/// The invisible field the system keyboard types into (control-page.md §3.1.1). To the input
/// method it is a plain text view: the committed text stays in it, so an input method that
/// rewrites what it committed earlier (豆包 / 讯飞-style voice input with 识别优化, system
/// dictation's corrections) finds it where it is and edits it there. After each complete edit
/// `ImeMirror` sends the committed text's change to the focus computer, so marked text (pinyin,
/// kana, voice recognition in progress) never goes out and a commit goes out once.
///
/// It is a `UITextView` so a line break lives in the text: Return types "\n" into the field (the
/// Return key on the computer), and a later Backspace or rewrite across it counts it there too.
/// (A `UITextField` cannot hold one; restarting the field at each Return instead would end what
/// a voice input method can still correct at every line break.)
///
/// The field keeps its text from the keyboard opening until it hides or something else types at
/// the computer's caret or moves it (`restart`: the page's keys, touchpad clicks, another text
/// field or computer, `FieldRestartRules`); then it starts empty and the input method is told
/// the text changed under it. A field grown past `maxField` does the same once nothing is marked.
///
/// Why (user 2026-11-12): the field used to send and empty itself after every commit, so the
/// input method's context was always empty and it could not correct text it had committed: a
/// voice input method's rewrite was appended a second time or lost.
class RemoteInputField: UITextView, UITextViewDelegate {
    weak var sink: RemoteInputSink?
    /// The committed text the focus computer has from this field, in front of its caret.
    private(set) var sent = ""
    /// Characters (UTF-16, as Android) kept before the field starts afresh once nothing is marked.
    static let maxField = 1000
    private var settleDue = false
    private var restarting = false

    init() {
        super.init(frame: CGRect(x: 0, y: 0, width: 1, height: 1), textContainer: nil)
        delegate = self
        isScrollEnabled = false
        backgroundColor = .clear
        autocorrectionType = .no
        autocapitalizationType = .none
        smartQuotesType = .no
        smartDashesType = .no
        smartInsertDeleteType = .no
        spellCheckingType = .no
        inlinePredictionType = .no
        returnKeyType = .default
        tintColor = .clear
        textColor = .clear
        accessibilityLabel = "输入到焦点电脑"
    }
    required init?(coder: NSCoder) { fatalError() }

    // MARK: Input method calls (logged; the field is read once the event is handled)

    override func insertText(_ text: String) {
        log("insertText n=\(text.utf16.count)")
        super.insertText(text)
        settleSoon()
    }

    override func setMarkedText(_ markedText: String?, selectedRange: NSRange) {
        log("setMarkedText n=\(markedText?.utf16.count ?? 0) sel=\(selectedRange.location),\(selectedRange.length)")
        super.setMarkedText(markedText, selectedRange: selectedRange)
        settleSoon()
    }

    override func unmarkText() {
        log("unmarkText")
        super.unmarkText()
        settleSoon()
    }

    override func replace(_ range: UITextRange, withText text: String) {
        let a = offset(from: beginningOfDocument, to: range.start)
        let b = offset(from: beginningOfDocument, to: range.end)
        log("replace \(a),\(b) n=\(text.utf16.count)")
        super.replace(range, withText: text)
        settleSoon()
    }

    /// Backspace on the empty field is the Backspace key on the computer (a held delete key calls
    /// this again for each repeat). With text in the field it edits the field, and so the
    /// computer; with the caret at the start of text in the field it deletes nothing, as in any
    /// editor (the computer's caret is behind that text, not in front of it).
    override func deleteBackward() {
        log("deleteBackward")
        if (text ?? "").isEmpty && markedTextRange == nil {
            settleNow()
            sink?.fieldBackspace()
            return
        }
        super.deleteBackward()
        settleSoon()
    }

    func textViewDidChange(_ textView: UITextView) { settleSoon() }

    func textViewDidChangeSelection(_ textView: UITextView) {
        guard !restarting else { return }
        log("selection")
    }

    func textViewDidBeginEditing(_ textView: UITextView) { log("begin") }

    /// The keyboard hid (or another input view took over): what the input method still had
    /// marked is abandoned input, not a commit, and the next keyboard starts on an empty field.
    func textViewDidEndEditing(_ textView: UITextView) {
        log("end")
        restart("hidden")
    }

    // MARK: Mirror

    /// A look at the field once the input method's current edit is over. An iOS input method may
    /// change the field several times while handling one tap (choosing a candidate replaces the
    /// marked composition, and after a partial choice marks the rest again), and each change
    /// calls in here. Read from inside those calls the field can show an in-between state with
    /// nothing marked (with the Chinese 10-key keyboard, choosing candidates sent several
    /// characters for one choice, user 2026-11-12), so it is read after the event that changed it
    /// has been handled.
    private func settleSoon() {
        guard !settleDue, !restarting else { return }
        settleDue = true
        DispatchQueue.main.async { [weak self] in self?.settleNow() }
    }

    /// The committed text's change goes out (tests call this for "the event is over").
    func settleNow() {
        settleDue = false
        guard !restarting else { return }
        let value = text ?? ""
        let step = ImeMirror.step(sent: sent, field: value, marked: markedOffsets)
        if step.backspaces > 0 || !step.text.isEmpty { log("out backspaces=\(step.backspaces) text=\(step.text.utf16.count)") }
        sent = step.sent
        for _ in 0..<step.backspaces { sink?.fieldBackspace() }
        if !step.text.isEmpty { sink?.fieldText(step.text) }
        if markedTextRange == nil && value.utf16.count > Self.maxField { restart("long") }
    }

    /// The focus computer's caret moved or its text changed where this field cannot follow: the
    /// field empties (what the computer has stays there), marked text is dropped, and the input
    /// method hears that the text changed under it, so it forgets what it was editing. Nothing
    /// when the field is already empty.
    func restart(_ why: String) {
        guard !restarting else { return }
        guard !(text ?? "").isEmpty || !sent.isEmpty || markedTextRange != nil else { return }
        log("restart \(why)")
        restarting = true
        defer { restarting = false }
        settleDue = false
        inputDelegate?.selectionWillChange(self)
        inputDelegate?.textWillChange(self)
        if markedTextRange != nil { unmarkText() }
        text = ""
        sent = ""
        inputDelegate?.textDidChange(self)
        inputDelegate?.selectionDidChange(self)
    }

    private var markedOffsets: NSRange? {
        guard let range = markedTextRange else { return nil }
        let a = offset(from: beginningOfDocument, to: range.start)
        let b = offset(from: beginningOfDocument, to: range.end)
        return NSRange(location: a, length: b - a)
    }

    /// One diagnostics line: `op` and the field then, in lengths and offsets.
    private func log(_ op: String) {
        ImeLog.line {
            let marked = markedOffsets.map { "\($0.location),\($0.location + $0.length)" } ?? "-"
            return "\(op) | len=\((text ?? "").utf16.count) sel=\(selectedRange.location),\(selectedRange.length) marked=\(marked) sent=\(sent.utf16.count)"
        }
    }
}
