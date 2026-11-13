package com.hachiwii.oink.control

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** Robolectric: TextParts uses android.icu's grapheme iterator (ICU4J here, ICU on the phone). */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class TextPartsTest {
    @Test fun lineBreaksAndTabsBecomeKeysInOrder() {
        assertEquals(listOf(TextPart.Text("a"), TextPart.Key(Hid.RETURN), TextPart.Text("b"), TextPart.Key(Hid.TAB), TextPart.Text("c"), TextPart.Key(Hid.RETURN)),
            TextParts.split("a\r\nb\tc\n"))
    }

    @Test fun otherControlCharactersAreDropped() {
        assertEquals(listOf(TextPart.Text("ab")), TextParts.split("a\u0007b"))
        assertEquals(listOf(TextPart.Text("a"), TextPart.Key(Hid.RETURN), TextPart.Text("b")), TextParts.split("a b"))
        assertEquals(emptyList<TextPart>(), TextParts.split(""))
    }

    @Test fun shortTextGoesWhole() {
        val text = "中".repeat(1365) // 4095 bytes
        assertEquals(listOf(TextPart.Text(text)), TextParts.split(text))
    }

    @Test fun longTextIsCutUnder4096BytesBetweenGraphemes() {
        // 6000 bytes of CJK, then emoji with a skin tone (8 bytes each) and ZWJ families (25 bytes each).
        val text = "中".repeat(2000) + "👍🏽".repeat(300) + "👨‍👩‍👧‍👦".repeat(200)
        val parts = TextParts.split(text).map { (it as TextPart.Text).text }
        assertTrue(parts.size > 1)
        parts.forEach { assertTrue(it.toByteArray(Charsets.UTF_8).size <= TextParts.MAX_BYTES) }
        assertEquals(text, parts.joinToString(""))
        // No part starts inside a cluster: never with a skin tone or a zero-width joiner.
        parts.forEach { assertTrue(!it.startsWith("🏽") && !it.startsWith("‍")) }
    }
}
