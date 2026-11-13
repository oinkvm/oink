package com.hachiwii.oink

import org.junit.Assert.*
import org.junit.Test

/** Only text is broadcast to every online device; everything else goes to one chosen device. */
class SharePolicyTest {
    @Test fun textUsesUtf8Bytes() {
        assertTrue(SharePolicy.canBroadcast(ShareDraft("a".repeat(32768))))
        assertFalse(SharePolicy.canBroadcast(ShareDraft("a".repeat(32769))))
        assertFalse(SharePolicy.canBroadcast(ShareDraft("汉".repeat(11000))))
        assertFalse(SharePolicy.canBroadcast(ShareDraft()))
    }
    @Test fun allPortableFormatsShareOneInlineBudget() {
        assertTrue(SharePolicy.canBroadcast(ShareDraft(text="a".repeat(16000),html="h".repeat(16000))))
        assertFalse(SharePolicy.canBroadcast(ShareDraft(text="a".repeat(20000),html="h".repeat(20000))))
    }
    @Test fun picturesAndFilesAreNeverBroadcast() {
        val png = LocalItem("unused", "picture.png", "image/png", 1024)
        assertFalse(SharePolicy.canBroadcast(ShareDraft(items = listOf(png))))
        assertFalse(SharePolicy.canBroadcast(ShareDraft("caption", listOf(png))))
        assertFalse(SharePolicy.canBroadcast(ShareDraft(items = listOf(png.copy(mime = "application/pdf")))))
    }
}
