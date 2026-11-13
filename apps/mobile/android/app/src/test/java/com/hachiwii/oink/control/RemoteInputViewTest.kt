package com.hachiwii.oink.control

import android.os.SystemClock
import android.view.KeyEvent
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.ExtractedTextRequest
import android.view.inputmethod.InputConnection
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowLog

/**
 * The system keyboard's hidden field (docs/control-page.md §3.1.1, user 2026-11-12): what reaches
 * the focus computer for scripted input-method call sequences. Composing text never goes out and
 * a commit goes out once, however the input method gets there; the field answers like a text
 * view, so an input method that rewrites text it committed earlier finds it where it is.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class RemoteInputViewTest {
    private val out = ArrayList<String>()
    private val sink = object : RemoteInputSink {
        override fun text(text: String) { out += "text $text" }
        override fun backspace() { out += "backspace" }
        override fun key(code: Int, down: Boolean) { out += "key ${Integer.toHexString(code)} $down" }
        override fun hardwareKey(keyCode: Int, down: Boolean) = false
    }
    private val view = RemoteInputView(ApplicationProvider.getApplicationContext(), sink)

    private fun field(): InputConnection = view.onCreateInputConnection(EditorInfo())

    private fun InputConnection.batch(block: InputConnection.() -> Unit) { beginBatchEdit(); block(); endBatchEdit() }
    private fun InputConnection.key(code: Int) {
        sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, code)); sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, code))
    }
    private fun backspaces(n: Int) = List(n) { "backspace" }

    @Test fun pinyinComposingIsNotSentAndTheCommitIsSentOnce() {
        val ic = field()
        ic.setComposingText("n", 1); ic.setComposingText("ni", 1); ic.setComposingText("nih", 1)
        assertEquals(emptyList<String>(), out)
        ic.commitText("你好", 1)
        ic.finishComposingText()
        assertEquals(listOf("text 你好"), out)
    }

    /** Gboard 9-key: digits / guessed letters as composing text, each key press replacing it. */
    @Test fun gboardNineKeyTapsThenCandidate() {
        val ic = field()
        for (c in listOf("6", "64", "644", "6442", "64426")) ic.batch { setComposingText(c, 1) }
        ic.batch { setComposingText("ni'hao", 1) }
        assertEquals(emptyList<String>(), out)
        ic.batch { commitText("你好", 1) }
        assertEquals(listOf("text 你好"), out)
    }

    /**
     * Sogou / Xiaomi 9-key: a candidate for part of the pinyin is committed in front of the rest in
     * one batch, the next candidate re-composes the committed one with the rest, and the last
     * commit carries both. Before the fix the re-composed "你" went out a second time.
     */
    @Test fun sogouNineKeyPartialCandidatesAreSentOnce() {
        val ic = field()
        ic.batch { setComposingText("nihao", 1) }
        // First tap: 你 for "ni", "hao" stays composing.
        ic.batch { commitText("你", 1); setComposingText("hao", 1) }
        assertEquals(listOf("text 你"), out)
        // The keyboard takes 你 back into its composition with the rest.
        ic.batch { setComposingRegion(0, 4); setComposingText("你hao", 1) }
        assertEquals(listOf("text 你"), out)
        // Second tap: 好; the whole phrase is committed.
        ic.batch { commitText("你好", 1) }
        assertEquals(listOf("text 你", "text 好"), out)
    }

    /** Xiaomi 9-key style per key press: finish, delete what was there, compose anew, in one batch. */
    @Test fun finishDeleteAndRecomposeInOneBatchSendsNothing() {
        val ic = field()
        ic.batch { setComposingText("g", 1) }
        ic.batch { finishComposingText(); deleteSurroundingText(1, 0); setComposingText("ha", 1) }
        ic.batch { finishComposingText(); deleteSurroundingText(2, 0); setComposingText("hao", 1) }
        assertEquals(emptyList<String>(), out)
        ic.batch { setComposingText("好", 1); finishComposingText() }
        assertEquals(listOf("text 好"), out)
    }

    /** An input method that commits the letters and then replaces them with the candidate. */
    @Test fun committedLettersReplacedByTheCandidateAreDeletedOnTheComputer() {
        val ic = field()
        ic.commitText("ni", 1)
        ic.batch { deleteSurroundingText(2, 0); commitText("你", 1) }
        assertEquals(listOf("text ni", "backspace", "backspace", "text 你"), out)
    }

    /**
     * 豆包 / Sogou voice input (user 2026-11-12): each sentence is recognised as composing text and
     * committed, then a language model rewrites it — the second time together with the end of the
     * first sentence. The input method finds the text by absolute offsets; the cursor stays at the
     * end, so each sentence goes after the last one, and a rewrite of text already on the computer
     * is Backspace from the end back to the first changed character, then the rest typed again.
     */
    @Test fun voiceSentencesAndTheirRewritesGoOutAtTheEnd() {
        val ic = field()
        ic.batch { setComposingText("今天", 1) }
        ic.batch { setComposingText("今天天气", 1) }
        assertEquals(emptyList<String>(), out)
        ic.batch { commitText("今天天气很好", 1) }
        assertEquals(listOf("text 今天天气很好"), out)
        // Rewrite of sentence 1 (a full stop added) over the committed text.
        ic.batch { setComposingRegion(0, 6); setComposingText("今天天气很好。", 1); finishComposingText() }
        assertEquals(listOf("text 今天天气很好", "text 。"), out)

        // Where the input method looks for the cursor: the end, by absolute offset.
        val extracted = ic.getExtractedText(ExtractedTextRequest(), 0)
        assertEquals("今天天气很好。", extracted.text.toString())
        assertEquals(0, extracted.startOffset)
        assertEquals(7, extracted.selectionStart); assertEquals(7, extracted.selectionEnd)
        val around = ic.getSurroundingText(3, 3, 0)!!
        assertEquals(4, around.offset)
        assertEquals("很好。", around.text.toString())
        assertEquals(3, around.selectionStart)

        // Sentence 2 streams at the cursor; nothing goes out until it is committed.
        ic.batch { setComposingText("我们", 1) }
        ic.batch { setComposingText("我们去公园", 1) }
        assertEquals(2, out.size)
        ic.batch { commitText("我们去公园吧", 1) }
        assertEquals("text 我们去公园吧", out.last())
        out.clear()

        // The rewrite spans both sentences: "很好。我们去公园吧" → "不错，我们去公园吧。" (API 34 replaceText).
        ic.replaceText(4, 13, "不错，我们去公园吧。", 1, null)
        assertEquals(backspaces(9) + "text 不错，我们去公园吧。", out)
        assertEquals("今天天气不错，我们去公园吧。", ic.getExtractedText(ExtractedTextRequest(), 0).text.toString())
        assertEquals("cursor stays at the end", 14, ic.getExtractedText(ExtractedTextRequest(), 0).selectionStart)
        out.clear()

        // Sentence 3 goes after it.
        ic.batch { setComposingText("明天", 1) }
        ic.batch { commitText("明天见", 1) }
        assertEquals(listOf("text 明天见"), out)
    }

    /** Rewrites by selection + commit, by deletion + commit, and by re-composing committed text. */
    @Test fun rewritesOfSentTextAreBackspacedFromTheEndAndRetyped() {
        val ic = field()
        ic.commitText("abc def", 1)
        ic.batch { setSelection(4, 7); commitText("xyz.", 1) }
        assertEquals(listOf("text abc def") + backspaces(3) + "text xyz.", out)
        out.clear()
        ic.batch { deleteSurroundingText(4, 0); commitText("XYZ!", 1) }
        assertEquals(backspaces(4) + "text XYZ!", out)
        out.clear()
        // Taken back into the composition: waits until the input method is done with it.
        ic.batch { setComposingRegion(0, 3) }
        ic.batch { setComposingText("ABC", 1) }
        assertEquals(emptyList<String>(), out)
        ic.batch { finishComposingText() }
        assertEquals(backspaces(8) + "text ABC XYZ!", out)
    }

    @Test fun mirrorCostIsEverythingFromTheFirstChangedCluster() {
        assertEquals(MirrorStep(5, "X def!", "abX def!"), ImeMirror.step("abc def", "abX def!", -1, -1))
        assertEquals(MirrorStep(1, "👍🏿", "a👍🏿"), ImeMirror.step("a👍🏽", "a👍🏿", -1, -1))
        assertEquals(MirrorStep(0, "", "abc"), ImeMirror.step("abc", "abXc", 2, 3))
    }

    @Test fun composingLeftWhenTheConnectionClosesIsNotSent() {
        val ic = field()
        ic.setComposingText("ni", 1)
        ic.closeConnection()
        assertEquals(emptyList<String>(), out)
    }

    @Test fun backspaceOnTheEmptyFieldAndWhenHeld() {
        val ic = field()
        ic.deleteSurroundingText(1, 0)
        ic.deleteSurroundingTextInCodePoints(1, 0)
        // Held Backspace from an AOSP-style keyboard: one down per repeat, one up at the end.
        ic.sendKeyEvent(KeyEvent(0, 0, KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_DEL, 0))
        ic.sendKeyEvent(KeyEvent(0, 400, KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_DEL, 1))
        ic.sendKeyEvent(KeyEvent(0, 450, KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_DEL, 2))
        ic.sendKeyEvent(KeyEvent(0, 470, KeyEvent.ACTION_UP, KeyEvent.KEYCODE_DEL, 0))
        assertEquals(backspaces(5), out)
    }

    @Test fun backspaceOverSentTextDeletesItOnTheComputer() {
        val ic = field()
        ic.commitText("ab", 1)
        ic.deleteSurroundingText(1, 0)                                                     // Gboard
        ic.sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_DEL))               // AOSP
        ic.sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_DEL))
        ic.deleteSurroundingText(1, 0)                                                     // now empty
        assertEquals(listOf("text ab", "backspace", "backspace", "backspace"), out)
    }

    /** The computer's caret is behind the field's text, so Backspace in front of it deletes nothing there either. */
    @Test fun backspaceAtTheStartOfTextInTheFieldStaysInTheField() {
        val ic = field()
        ic.commitText("ab", 1)
        ic.setSelection(0, 0)
        ic.deleteSurroundingText(1, 0)
        ic.deleteSurroundingTextInCodePoints(1, 0)
        ic.key(KeyEvent.KEYCODE_DEL)
        assertEquals(listOf("text ab"), out)
    }

    @Test fun deletingAnEmojiIsOneBackspace() {
        val ic = field()
        ic.commitText("a👍🏽", 1)
        ic.deleteSurroundingText("👍🏽".length, 0)
        assertEquals(listOf("text a👍🏽", "backspace"), out)
    }

    /** Return and Tab are typed into the field (the keys on the computer), so Backspace and rewrites count them. */
    @Test fun returnAndTabAreTypedIntoTheFieldInOrder() {
        val ic = field()
        ic.batch { commitText("ok", 1); sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_ENTER)) }
        ic.sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_ENTER))
        ic.key(KeyEvent.KEYCODE_7)                                                         // digits as key events
        ic.deleteSurroundingText(2, 0)                                                     // "7" and the line break
        ic.performEditorAction(EditorInfo.IME_ACTION_DONE)
        ic.key(KeyEvent.KEYCODE_TAB)
        assertEquals(listOf("text ok\n", "text 7", "backspace", "backspace", "text \n", "text \t"), out)
        assertEquals("ok\n\t", ic.getExtractedText(ExtractedTextRequest(), 0).text.toString())
    }

    /** An arrow (or Esc, a shortcut…) from the input method moves the computer's caret: the field starts afresh. */
    @Test fun keysTheFieldCannotFollowGoAsKeysAndRestartIt() {
        val ic = field()
        ic.commitText("ab", 1)
        ic.key(KeyEvent.KEYCODE_DPAD_LEFT)
        assertEquals("", ic.getExtractedText(ExtractedTextRequest(), 0).text.toString())
        ic.commitText("c", 1)
        ic.deleteSurroundingText(1, 0)
        ic.deleteSurroundingText(1, 0)
        val left = Integer.toHexString(Hid.LEFT)
        assertEquals(listOf("text ab", "key $left true", "key $left false", "text c", "backspace", "backspace"), out)
    }

    /** The page moved the computer's caret (its own keys, a click, another field): composing is dropped, the field is empty. */
    @Test fun restartFromThePageEmptiesTheField() {
        val ic = field()
        ic.commitText("ab", 1)
        ic.setComposingText("ni", 1)
        view.restartField()
        assertEquals("", ic.getExtractedText(ExtractedTextRequest(), 0).text.toString())
        ic.commitText("你", 1)
        assertEquals(listOf("text ab", "text 你"), out)
    }

    /** The diagnostics name the calls with lengths and offsets, never the text. */
    @Test fun diagnosticsCarryNoText() {
        SystemClock.sleep(1100)
        ShadowLog.reset()
        val ic = field()
        ic.batch { setComposingText("mima", 1) }
        ic.batch { commitText("秘密", 1) }
        ic.getTextBeforeCursor(10, 0)
        ic.getExtractedText(ExtractedTextRequest(), 0)
        val lines = ShadowLog.getLogsForTag("OinkIme").map { it.msg }
        assertTrue(lines.any { it.contains("commitText n=2 pos=1") })
        assertTrue(lines.any { it.contains("out backspaces=0 text=2") })
        assertFalse(lines.any { it.contains("秘") || it.contains("mima") })
    }
}
