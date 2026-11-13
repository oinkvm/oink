package com.hachiwii.oink.privilege;

import android.content.ClipData;
import android.content.ClipDescription;
import android.os.Build;
import android.os.PersistableBundle;

/**
 * The Oink clip marker on an Android clip (oink_foundation::clipboard_marker): the bytes come
 * from the Rust core unchanged and are only carried here. Shared by the app's own writes and the
 * privileged helper's writes and reads.
 *
 * It is a ClipDescription extra, not a clip item: TextView (and its OnReceiveContentListener)
 * pastes every item of a clip, joined by newlines, so an extra item would be pasted into other
 * apps. Extras are invisible to paste. Hex because a PersistableBundle has no byte arrays.
 */
public final class ClipMarker {
    /** oink_foundation::clipboard_marker::MARKER_ANDROID_EXTRA */
    public static final String EXTRA = "com.hachiwii.oink.clip";
    private static final char[] HEX = "0123456789abcdef".toCharArray();
    private ClipMarker() {}

    /** Sets the clip's description extras: the marker (if any) and the sensitivity flag. */
    public static void describe(ClipData clip, byte[] marker, boolean sensitive) {
        PersistableBundle extras = new PersistableBundle();
        if (marker != null) {
            char[] hex = new char[marker.length * 2];
            for (int i = 0; i < marker.length; i++) {
                hex[i * 2] = HEX[(marker[i] >> 4) & 0xf];
                hex[i * 2 + 1] = HEX[marker[i] & 0xf];
            }
            extras.putString(EXTRA, new String(hex));
        }
        if (sensitive && Build.VERSION.SDK_INT >= 33) extras.putBoolean(ClipDescription.EXTRA_IS_SENSITIVE, true);
        if (!extras.isEmpty()) clip.getDescription().setExtras(extras);
    }

    /** The marker bytes on a clip, or null. The core validates them. */
    public static byte[] read(ClipDescription description) {
        PersistableBundle extras = description.getExtras();
        String hex = extras == null ? null : extras.getString(EXTRA);
        if (hex == null || hex.length() % 2 != 0) return null;
        byte[] bytes = new byte[hex.length() / 2];
        for (int i = 0; i < bytes.length; i++) {
            int high = Character.digit(hex.charAt(i * 2), 16), low = Character.digit(hex.charAt(i * 2 + 1), 16);
            if (high < 0 || low < 0) return null;
            bytes[i] = (byte) ((high << 4) | low);
        }
        return bytes;
    }
}
