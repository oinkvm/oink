package com.hachiwii.oink.privilege;

import android.os.SystemClock;
import android.view.InputDevice;
import android.view.InputEvent;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;

/** Shell-authorized system actions for completed desktop gestures; not raw touch emulation. */
final class SystemGestures {
    private SystemGestures() {}
    static void perform(String gesture) throws Exception {
        if (gesture == null) throw new IllegalArgumentException("gesture");
        switch (gesture) {
            case "home": press(KeyEvent.KEYCODE_HOME, 0); break;
            case "recents": press(KeyEvent.KEYCODE_APP_SWITCH, 0); break;
            case "switch_next": switchApp(false); break;
            case "switch_previous": switchApp(true); break;
            case "notifications": {
                // StatusBarShellCommand is the framework's shell entry point. Unlike
                // AccessibilityService global actions this needs no enabled service.
                Process command = new ProcessBuilder("/system/bin/cmd", "statusbar", "expand-notifications")
                    .redirectErrorStream(true).start();
                try {
                    // Drain output before waitFor so a failed binder call cannot fill the pipe.
                    while (command.getInputStream().read() != -1) { }
                    if (command.waitFor() != 0) throw new IllegalStateException("notification action rejected");
                } finally { command.destroy(); }
                break;
            }
            default: throw new IllegalArgumentException("gesture");
        }
    }
    private static void switchApp(boolean previous) throws Exception {
        int meta = KeyEvent.META_ALT_ON | KeyEvent.META_ALT_LEFT_ON;
        try {
            key(KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_ALT_LEFT, meta);
            if (previous) {
                meta |= KeyEvent.META_SHIFT_ON | KeyEvent.META_SHIFT_LEFT_ON;
                try {
                    key(KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_SHIFT_LEFT, meta);
                    press(KeyEvent.KEYCODE_TAB, meta);
                } finally { key(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_SHIFT_LEFT, KeyEvent.META_ALT_ON | KeyEvent.META_ALT_LEFT_ON); }
            } else press(KeyEvent.KEYCODE_TAB, meta);
        } finally { key(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_ALT_LEFT, 0); }
    }
    private static void press(int code, int meta) throws Exception {
        try { key(KeyEvent.ACTION_DOWN, code, meta); }
        finally { key(KeyEvent.ACTION_UP, code, meta); }
    }
    private static void key(int action, int code, int meta) throws Exception {
        long now = SystemClock.uptimeMillis();
        KeyEvent event = new KeyEvent(now, now, action, code, 0, meta,
            KeyCharacterMap.VIRTUAL_KEYBOARD, 0, KeyEvent.FLAG_FROM_SYSTEM, InputDevice.SOURCE_KEYBOARD);
        Object manager = Class.forName("android.hardware.input.InputManager").getMethod("getInstance").invoke(null);
        // WAIT_FOR_FINISH preserves DOWN/UP order and makes action failure visible to the owner.
        boolean accepted = (boolean) manager.getClass().getMethod("injectInputEvent", InputEvent.class, int.class)
            .invoke(manager, event, 2);
        if (!accepted) throw new IllegalStateException("system gesture key rejected");
    }
}
