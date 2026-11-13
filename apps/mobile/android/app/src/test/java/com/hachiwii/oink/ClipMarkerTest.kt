package com.hachiwii.oink

import android.content.ClipData
import android.content.ClipDescription
import android.os.Parcel
import org.robolectric.RuntimeEnvironment
import com.hachiwii.oink.privilege.ClipMarker
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** Oink's clipboard writes carry the core's clip marker without changing what other apps paste. */
@RunWith(RobolectricTestRunner::class)
// A plain Application: OinkApplication would start the connection service and load the Rust core.
@Config(sdk = [34], application = android.app.Application::class)
class ClipMarkerTest {
    private val context = RuntimeEnvironment.getApplication()
    // Shape of the core's bytes (OKC1 + identity); the hosts carry them unchanged.
    private val marker = "OKC1".toByteArray() + ByteArray(48) { (it * 7 - 100).toByte() }

    /** What TextView pastes: every item of the clip, joined by newlines. */
    private fun pasted(clip: ClipData) = (0 until clip.itemCount).joinToString("\n") { clip.getItemAt(it).coerceToText(context) }

    private fun parcelled(clip: ClipData): ClipData {
        val parcel = Parcel.obtain()
        try {
            clip.writeToParcel(parcel, 0)
            parcel.setDataPosition(0)
            return ClipData.CREATOR.createFromParcel(parcel)
        } finally { parcel.recycle() }
    }

    @Test fun receivedTextCarriesTheMarkerAndPastesUnchanged() {
        val clip = ClipboardSink.clip(context, ReceivedContent(text = "from pc\n", sensitive = true, marker = marker))
        assertEquals(1, clip.itemCount)
        assertEquals("from pc\n", pasted(clip))
        assertArrayEquals(arrayOf(ClipDescription.MIMETYPE_TEXT_PLAIN), (0 until clip.description.mimeTypeCount).map { clip.description.getMimeType(it) }.toTypedArray())
        // Through the clipboard service (a parcel) the marker and the sensitivity flag survive.
        val stored = parcelled(clip)
        assertArrayEquals(marker, ClipMarker.read(stored.description))
        assertTrue(stored.description.extras.getBoolean(ClipDescription.EXTRA_IS_SENSITIVE))
        assertEquals("from pc\n", pasted(stored))
    }

    @Test fun contentWithoutMarkerIsWrittenAsBefore() {
        val clip = ClipboardSink.clip(context, ReceivedContent(text = "shared"))
        assertNull(clip.description.extras)
        assertNull(ClipMarker.read(clip.description))
        assertEquals("shared", pasted(clip))
    }

    /** The privileged helper's write (ShellClipboard.write) and its read-back (ShellClipboard.read). */
    @Test fun helperWriteAndReadBackShareTheMarker() {
        val clip = ClipData.newPlainText("Oink", "helper text")
        ClipMarker.describe(clip, marker, false)
        val stored = parcelled(clip)
        assertEquals(1, stored.itemCount)
        assertEquals("helper text", pasted(stored))
        assertArrayEquals(marker, ClipMarker.read(stored.description))
        assertFalse(stored.description.extras.containsKey(ClipDescription.EXTRA_IS_SENSITIVE))
    }

    @Test fun anotherAppsClipHasNoMarker() {
        val clip = ClipData.newPlainText("other", "copied elsewhere")
        assertNull(ClipMarker.read(clip.description))
    }
}
