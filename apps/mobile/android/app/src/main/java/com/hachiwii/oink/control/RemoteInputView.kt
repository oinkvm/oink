package com.hachiwii.oink.control

import android.annotation.SuppressLint
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.icu.text.BreakIterator
import android.os.Bundle
import android.os.SystemClock
import android.text.Editable
import android.text.InputType
import android.text.Selection
import android.text.SpannableStringBuilder
import android.util.Log
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.CompletionInfo
import android.view.inputmethod.CorrectionInfo
import android.view.inputmethod.CursorAnchorInfo
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.ExtractedText
import android.view.inputmethod.ExtractedTextRequest
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputContentInfo
import android.view.inputmethod.InputMethodManager
import android.view.inputmethod.SurroundingText
import android.view.inputmethod.TextAttribute

/** What the hidden field hands over: committed text, Backspace, other keys, hardware keys. */
interface RemoteInputSink {
    /** Committed text; a line break or tab in it is the Return / Tab key ([TextParts]). */
    fun text(text: String)
    /** One Backspace (a deleted committed character, or Backspace on the empty field). */
    fun backspace()
    /** A key the input method sent as a key event (arrows, Esc…): down / up as it sent them. */
    fun key(code: Int, down: Boolean)
    /** A key from a hardware keyboard; false when it has no remote equivalent (the view lets the system have it). */
    fun hardwareKey(keyCode: Int, down: Boolean): Boolean
}

/**
 * What to send so the focus computer shows the field's committed text again: [backspaces] grapheme
 * clusters off the end of what it has, then [text]; [sent] is what it then has.
 */
data class MirrorStep(val backspaces: Int, val text: String, val sent: String)

/**
 * The field's committed text as the focus computer has it (docs/control-page.md §3.1.1, user
 * 2026-11-12). Only text outside the composing region counts. While the input method composes,
 * text that only grows the committed part is sent (a candidate committed in front of the rest of
 * the pinyin); anything else waits until composing ends, because the input method has taken
 * already sent text back into its composition and normally commits it again unchanged. Without
 * composing text, the difference to what was sent goes out: Backspace for each grapheme cluster
 * that is gone from the end of the common part, then the new text.
 *
 * The computer's caret is at the end of what was sent: the field only ever types there, and
 * anything that moves the caret on the computer restarts the field ([RemoteInputView.restartField]).
 * So an edit that is not at the end — a voice input method rewriting the last sentences after
 * recognition, a candidate replacing a word further back — costs a Backspace for every cluster
 * from the first changed one to the end, and the rest typed again from there. Nothing assumes
 * where the caret is beyond that (no arrow keys: how far one moves differs between apps).
 */
object ImeMirror {
    fun step(sent: String, field: String, composingStart: Int, composingEnd: Int): MirrorStep {
        val start = minOf(composingStart, composingEnd)
        val end = maxOf(composingStart, composingEnd)
        if (start >= 0 && end > start) {
            val committed = field.substring(0, start.coerceAtMost(field.length))
            return if (committed.startsWith(sent)) MirrorStep(0, committed.substring(sent.length), committed) else MirrorStep(0, "", sent)
        }
        if (field == sent) return MirrorStep(0, "", sent)
        var k = 0
        val max = minOf(sent.length, field.length)
        while (k < max && sent[k] == field[k]) k++
        // Back to a cluster start of what was sent: Backspace removes whole clusters there.
        val it = BreakIterator.getCharacterInstance().apply { setText(sent) }
        if (k < sent.length && !it.isBoundary(k)) k = it.preceding(k).coerceAtLeast(0)
        return MirrorStep(clusters(sent.substring(k)), field.substring(k), field)
    }

    private fun clusters(s: String): Int {
        if (s.isEmpty()) return 0
        val it = BreakIterator.getCharacterInstance().apply { setText(s) }
        var n = 0
        while (it.next() != BreakIterator.DONE) n++
        return n
    }
}

/**
 * Diagnostics of what the input method does with the field (tag OinkIme, user 2026-11-12): one line
 * per input-connection call and per edit sent to the computer, with lengths and offsets only, never
 * the text. At most [PER_SECOND] lines a second; a line says how many were left out.
 */
internal object ImeLog {
    private const val TAG = "OinkIme"
    private const val PER_SECOND = 100
    private var window = Long.MIN_VALUE / 2
    private var lines = 0
    private var dropped = 0

    fun line(message: () -> String) {
        val now = SystemClock.uptimeMillis()
        if (now - window >= 1000) {
            if (dropped > 0) Log.i(TAG, "($dropped lines left out)")
            window = now; lines = 0; dropped = 0
        }
        if (lines >= PER_SECOND) { dropped++; return }
        lines++
        Log.i(TAG, message())
    }
}

/**
 * The invisible field the system keyboard types into (docs/control-page.md §3.1.1). To the input
 * method it is a plain text field: the text stays in it, selection, composing and extracted-text
 * changes are reported back as from a text view, and it answers every question an input method
 * asks about its content with absolute offsets (extracted text, surrounding text, cursor anchor
 * info). After each complete edit (the input method's batch edits are applied whole) [ImeMirror]
 * sends the committed text's change to the focus computer, so composing text (pinyin, 9-key
 * letters, kana, voice recognition in progress…) never goes out and a commit goes out once.
 *
 * The field keeps its text for one input session: from the keyboard opening until it hides, the
 * input method restarts the connection, or something else moves the computer's caret or types
 * there ([restartField]: keys the input method sends that the field cannot follow, the page's own
 * keys, a touchpad click, another text field or computer). Then it starts empty again and the input
 * method is restarted, since what it knows about the text is no longer what is in front of the
 * computer's caret. A field grown past [MAX_FIELD] does the same once nothing is composing.
 *
 * Why (user 2026-11-12, 豆包 voice input on a Phone, which rewrites recognised text with a
 * language model): the field used to answer getExtractedText with nothing, getSurroundingText
 * without an offset and cursor updates not at all, so an input method that finds its text by
 * absolute position saw the cursor at 0 — right while the field was empty, so the first sentence
 * went out fine, and the second one was put in front of the first (or replaced part of it), which
 * the mirror then faithfully typed on the computer. Return also went to the computer as a key
 * without being in the field, so a later rewrite deleted one character too few there, and
 * Backspace with the cursor at the start of a non-empty field went to the computer although the
 * text it would have deleted was the field's.
 *
 * (Before that it sent and emptied the field on every commit / finishComposingText call. Chinese
 * 9-key keyboards finish, delete and re-compose text inside one batch edit, and re-compose a
 * committed candidate together with the rest of the pinyin; each of those calls then sent a piece,
 * and the field the input method was editing had been emptied under it, so its later commit typed
 * the text again.)
 *
 * Return and Tab are typed into the field (a line break / tab in the text, the key on the
 * computer), so the field keeps matching what is in front of the caret. Backspace with an empty
 * field becomes the key; with text in the field it only edits the field (and so the computer). No
 * suggestions, no extract UI or fullscreen editor in landscape.
 */
@SuppressLint("ViewConstructor")
class RemoteInputView(context: Context, private val sink: RemoteInputSink) : View(context) {
    init {
        isFocusable = true
        isFocusableInTouchMode = true
        contentDescription = "输入到焦点电脑"
        importantForAutofill = IMPORTANT_FOR_AUTOFILL_NO
    }

    override fun onCheckIsTextEditor() = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_ACTION_NONE
        // Each connection starts with an empty field, and the input method is told so.
        outAttrs.initialSelStart = 0
        outAttrs.initialSelEnd = 0
        outAttrs.setInitialSurroundingText("")
        restartDue = false
        return Connection(++connections).also { live = it; ImeLog.line { "c${it.id} open" } }
    }

    private val imm get() = context.getSystemService(InputMethodManager::class.java)
    /** The input method's current connection (null while none is open). */
    private var live: Connection? = null
    private var connections = 0
    private var restartDue = false

    fun showKeyboard() {
        requestFocus()
        imm.showSoftInput(this, 0)
    }

    fun hideKeyboard() {
        imm.hideSoftInputFromWindow(windowToken, 0)
        clearFocus()
    }

    /**
     * The focus computer's caret moved or its text changed where this field cannot follow (a key
     * from the page, a touchpad click, another field or computer): the field empties, and the
     * input method is restarted so it forgets the text it was editing. Nothing when the field is
     * already empty.
     */
    fun restartField() { live?.restart() }

    /** After the current call: the input method then starts on the empty field. */
    private fun restartInput() {
        if (restartDue) return
        restartDue = true
        post { if (restartDue) { restartDue = false; imm.restartInput(this) } }
    }

    private inner class Connection(val id: Int) : BaseInputConnection(this@RemoteInputView, true) {
        /**
         * The field's text. Its own: BaseInputConnection's stand-in editable counts as "no real text"
         * there, and getSurroundingText then answers without the text's offset.
         */
        private val field: Editable = SpannableStringBuilder().also { Selection.setSelection(it, 0) }
        /** Open batch edits (the input method's own and BaseInputConnection's around each change). */
        private var depth = 0
        /** The committed text the focus computer has from this field, in front of its caret. */
        private var sent = ""
        /** Selection and composing region last reported to the input method. */
        private var reported = intArrayOf(0, 0, -1, -1)
        private var reportedText = ""
        /** The input method's extracted-text monitor (its request token), or null. */
        private var extractToken: Int? = null
        private var cursorMonitor = false
        private var closed = false

        override fun getEditable(): Editable = field

        private fun selStart() = Selection.getSelectionStart(field)
        private fun selEnd() = Selection.getSelectionEnd(field)
        private fun composing() = BaseInputConnection.getComposingSpanStart(field) to BaseInputConnection.getComposingSpanEnd(field)

        /** One diagnostics line: [op] (logged before an edit, after a query) and the field then, in lengths and offsets. */
        private fun log(op: String) = ImeLog.line {
            val (cs, ce) = composing()
            "c$id $op | len=${field.length} sel=${selStart()},${selEnd()} comp=$cs,$ce sent=${sent.length} depth=$depth${if (closed) " closed" else ""}"
        }

        override fun beginBatchEdit(): Boolean { depth++; log("beginBatchEdit"); return true }

        override fun endBatchEdit(): Boolean {
            if (depth > 0) depth--
            log("endBatchEdit")
            if (depth == 0) settle()
            return depth > 0
        }

        override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
            log("commitText n=${text?.length} pos=$newCursorPosition")
            return super.commitText(text, newCursorPosition)
        }

        override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
            log("setComposingText n=${text?.length} pos=$newCursorPosition")
            return super.setComposingText(text, newCursorPosition)
        }

        override fun setComposingRegion(start: Int, end: Int): Boolean {
            log("setComposingRegion $start,$end")
            return super.setComposingRegion(start, end)
        }

        override fun finishComposingText(): Boolean {
            log("finishComposingText")
            return super.finishComposingText()
        }

        override fun replaceText(start: Int, end: Int, text: CharSequence, newCursorPosition: Int, textAttribute: TextAttribute?): Boolean {
            log("replaceText $start,$end n=${text.length} pos=$newCursorPosition")
            return super.replaceText(start, end, text, newCursorPosition, textAttribute)
        }

        override fun setSelection(start: Int, end: Int): Boolean {
            log("setSelection $start,$end")
            val ok = super.setSelection(start, end)
            settle()
            return ok
        }

        override fun commitCorrection(correctionInfo: CorrectionInfo?): Boolean {
            // As a text view: the correction is already in the text; it would only be highlighted.
            log("commitCorrection offset=${correctionInfo?.offset} old=${correctionInfo?.oldText?.length} new=${correctionInfo?.newText?.length}")
            return true
        }

        override fun commitCompletion(text: CompletionInfo?): Boolean {
            log("commitCompletion n=${text?.text?.length}")
            return super.commitCompletion(text)
        }

        override fun commitContent(inputContentInfo: InputContentInfo, flags: Int, opts: Bundle?): Boolean {
            log("commitContent flags=$flags")
            return super.commitContent(inputContentInfo, flags, opts)
        }

        override fun performPrivateCommand(action: String?, data: Bundle?): Boolean {
            log("performPrivateCommand $action")
            return super.performPrivateCommand(action, data)
        }

        override fun performSpellCheck(): Boolean {
            log("performSpellCheck")
            return super.performSpellCheck()
        }

        override fun getTextBeforeCursor(length: Int, flags: Int): CharSequence? =
            super.getTextBeforeCursor(length, flags).also { log("getTextBeforeCursor $length -> ${it?.length}") }

        override fun getTextAfterCursor(length: Int, flags: Int): CharSequence? =
            super.getTextAfterCursor(length, flags).also { log("getTextAfterCursor $length -> ${it?.length}") }

        override fun getSelectedText(flags: Int): CharSequence? =
            super.getSelectedText(flags).also { log("getSelectedText -> ${it?.length}") }

        override fun getSurroundingText(beforeLength: Int, afterLength: Int, flags: Int): SurroundingText? =
            super.getSurroundingText(beforeLength, afterLength, flags).also {
                log("getSurroundingText $beforeLength,$afterLength -> ${it?.text?.length} at ${it?.offset} sel=${it?.selectionStart},${it?.selectionEnd}")
            }

        /** The whole field with its selection, as a text view answers; [flags] may ask to be told of every change. */
        override fun getExtractedText(request: ExtractedTextRequest?, flags: Int): ExtractedText {
            if (flags and InputConnection.GET_EXTRACTED_TEXT_MONITOR != 0) {
                extractToken = request?.token ?: 0
                reportedText = field.toString()
            }
            log("getExtractedText flags=$flags token=${request?.token} hint=${request?.hintMaxChars}")
            return extract(request)
        }

        private fun extract(request: ExtractedTextRequest?) = ExtractedText().apply {
            text = if (request != null && request.flags and InputConnection.GET_TEXT_WITH_STYLES != 0) SpannableStringBuilder(field) else field.toString()
            startOffset = 0
            partialStartOffset = -1
            partialEndOffset = -1
            selectionStart = selStart()
            selectionEnd = selEnd()
        }

        override fun requestCursorUpdates(cursorUpdateMode: Int): Boolean {
            log("requestCursorUpdates mode=$cursorUpdateMode")
            cursorMonitor = cursorUpdateMode and InputConnection.CURSOR_UPDATE_MONITOR != 0
            if (cursorUpdateMode and InputConnection.CURSOR_UPDATE_IMMEDIATE != 0) imm.updateCursorAnchorInfo(this@RemoteInputView, anchor())
            return true
        }

        override fun requestCursorUpdates(cursorUpdateMode: Int, cursorUpdateFilter: Int) = requestCursorUpdates(cursorUpdateMode)

        /** Selection and composing text; no geometry (the field is not drawn). */
        private fun anchor(): CursorAnchorInfo {
            val (cs, ce) = composing()
            return CursorAnchorInfo.Builder().apply {
                setSelectionRange(selStart(), selEnd())
                if (cs >= 0 && ce > cs) setComposingText(cs, field.subSequence(cs, ce))
                setMatrix(null)
            }.build()
        }

        override fun closeConnection() {
            log("closeConnection")
            depth = 0
            settle()
            // The framework finishes the composing text as it closes: that is abandoned input
            // (keyboard closed, input restarted), not a commit, so nothing goes out from here on.
            closed = true
            if (live === this) live = null
            super.closeConnection()
        }

        /**
         * A complete edit: the committed text's change goes out, and the input method hears the new
         * selection, composing region, extracted text and cursor (as from a text view). [force]: a
         * key the input method sends inside a batch edit; what it committed before the key goes out
         * first.
         */
        private fun settle(force: Boolean = false) {
            if (closed || (depth > 0 && !force)) return
            val (cs, ce) = composing()
            val step = ImeMirror.step(sent, field.toString(), cs, ce)
            if (step.backspaces > 0 || step.text.isNotEmpty()) log("out backspaces=${step.backspaces} text=${step.text.length}")
            sent = step.sent
            repeat(step.backspaces) { sink.backspace() }
            if (step.text.isNotEmpty()) sink.text(step.text)
            if (depth > 0) return
            report()
            if (cs < 0 && field.length > MAX_FIELD) restart()
        }

        private fun report() {
            val (cs, ce) = composing()
            val now = intArrayOf(selStart(), selEnd(), cs, ce)
            val text = field.toString()
            val moved = !now.contentEquals(reported)
            val changed = text != reportedText
            if (moved) {
                reported = now
                imm.updateSelection(this@RemoteInputView, now[0], now[1], now[2], now[3])
            }
            if (changed) {
                reportedText = text
                extractToken?.let { imm.updateExtractedText(this@RemoteInputView, it, extract(null)) }
            }
            if (cursorMonitor && (moved || changed)) imm.updateCursorAnchorInfo(this@RemoteInputView, anchor())
        }

        /** [RemoteInputView.restartField]: empty from here on (what the computer has stays there), the input method restarts. */
        fun restart() {
            if (closed || (field.isEmpty() && sent.isEmpty())) return
            log("restart")
            BaseInputConnection.removeComposingSpans(field)
            field.clear()
            Selection.setSelection(field, 0)
            sent = ""
            if (depth == 0) report()
            restartInput()
        }

        private fun insert(text: String) {
            beginBatchEdit()
            val a = minOf(selStart(), selEnd()).coerceAtLeast(0)
            val b = maxOf(selStart(), selEnd()).coerceAtLeast(0)
            field.replace(a, b, text)
            Selection.setSelection(field, a + text.length)
            endBatchEdit()
        }

        /** Backspace as a key: a character here goes (and so on the computer); with an empty field, Backspace itself. */
        private fun backspaceKey() {
            val a = selStart(); val b = selEnd()
            when {
                a != b && a >= 0 && b >= 0 -> { beginBatchEdit(); field.delete(minOf(a, b), maxOf(a, b)); endBatchEdit() }
                a > 0 -> {
                    val it = BreakIterator.getCharacterInstance().apply { setText(field.toString()) }
                    beginBatchEdit(); field.delete(it.preceding(a).coerceAtLeast(0), a); endBatchEdit()
                }
                field.isEmpty() -> { settle(force = true); sink.backspace() }
                // The cursor at the start of text in the field: nothing to delete, as in any editor
                // (the computer's caret is behind that text, not in front of it).
            }
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            log("deleteSurroundingText $beforeLength,$afterLength")
            // Backspace on the empty field goes to the computer as the key.
            if (beforeLength > 0 && field.isEmpty()) { settle(force = true); sink.backspace(); return true }
            return super.deleteSurroundingText(beforeLength, afterLength)
        }

        override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean {
            log("deleteSurroundingTextInCodePoints $beforeLength,$afterLength")
            if (beforeLength > 0 && field.isEmpty()) { settle(force = true); sink.backspace(); return true }
            return super.deleteSurroundingTextInCodePoints(beforeLength, afterLength)
        }

        override fun performEditorAction(editorAction: Int): Boolean {
            log("performEditorAction $editorAction")
            insert("\n")
            return true
        }

        override fun performContextMenuAction(id: Int): Boolean {
            log("performContextMenuAction $id")
            val a = minOf(selStart(), selEnd()).coerceAtLeast(0)
            val b = maxOf(selStart(), selEnd()).coerceAtLeast(0)
            val clipboard = context.getSystemService(ClipboardManager::class.java)
            return when (id) {
                android.R.id.selectAll -> setSelection(0, field.length)
                android.R.id.copy -> { if (a < b) clipboard?.setPrimaryClip(ClipData.newPlainText(null, field.substring(a, b))); true }
                android.R.id.cut -> {
                    if (a < b) {
                        clipboard?.setPrimaryClip(ClipData.newPlainText(null, field.substring(a, b)))
                        beginBatchEdit(); field.delete(a, b); endBatchEdit()
                    }
                    true
                }
                android.R.id.paste, android.R.id.pasteAsPlainText -> {
                    val clip = clipboard?.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(context)?.toString()
                    if (!clip.isNullOrEmpty()) commitText(clip, 1)
                    true
                }
                else -> false
            }
        }

        override fun sendKeyEvent(event: KeyEvent): Boolean {
            log("sendKeyEvent key=${event.keyCode} action=${event.action} repeat=${event.repeatCount} meta=${Integer.toHexString(event.metaState)}")
            val down = event.action == KeyEvent.ACTION_DOWN
            val combined = event.metaState and (KeyEvent.META_CTRL_ON or KeyEvent.META_ALT_ON or KeyEvent.META_META_ON) != 0
            when (event.keyCode) {
                // Soft keyboards send Backspace / Enter as key events, held ones once per repeat.
                KeyEvent.KEYCODE_DEL -> { if (down) backspaceKey(); return true }
                KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> if (!combined) { if (down) insert("\n"); return true }
                KeyEvent.KEYCODE_TAB -> if (!combined && !event.isShiftPressed) { if (down) insert("\t"); return true }
            }
            // Characters sent as key events (AOSP keyboards send digits that way) are committed text.
            val ch = event.unicodeChar
            if (ch != 0 && !Character.isISOControl(ch) && !combined) {
                if (down) commitText(String(Character.toChars(ch)), 1)
                return true
            }
            // Other keys with a computer equivalent (arrows, Esc, shortcuts…) go as keys. They move
            // the computer's caret or change its text where the field cannot follow: it restarts.
            val code = Hid.fromAndroid(event.keyCode) ?: return super.sendKeyEvent(event)
            if (event.action == KeyEvent.ACTION_DOWN || event.action == KeyEvent.ACTION_UP) {
                settle(force = true)
                sink.key(code, down)
                if (down && !Hid.keepsCaret(code)) restart()
            }
            return true
        }
    }

    private fun hardware(event: KeyEvent) = event.deviceId != KeyCharacterMap.VIRTUAL_KEYBOARD && event.source and android.view.InputDevice.SOURCE_KEYBOARD != 0

    // A hardware key's auto-repeat (further downs with repeatCount > 0) goes out too: the core sends
    // those to the focus computer as key repeats.
    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean =
        if (hardware(event)) sink.hardwareKey(keyCode, true) || super.onKeyDown(keyCode, event)
        else super.onKeyDown(keyCode, event)

    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean =
        if (hardware(event)) sink.hardwareKey(keyCode, false) || super.onKeyUp(keyCode, event) else super.onKeyUp(keyCode, event)

    companion object {
        /** Characters kept before the field starts afresh (input method restarted) once nothing is composing. */
        const val MAX_FIELD = 1000
    }
}
