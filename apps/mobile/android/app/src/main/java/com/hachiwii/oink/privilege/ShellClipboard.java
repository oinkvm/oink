package com.hachiwii.oink.privilege;

import android.content.ClipData;
import android.os.*;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.util.function.Consumer;

final class ShellClipboard implements AutoCloseable {
    private final Object service;
    private final Object listener;
    private final Consumer<Bundle> changed;
    private long revision;
    private boolean registered;
    ShellClipboard(Consumer<Bundle> changed) throws Exception {
        this.changed = changed;
        IBinder binder = (IBinder) Class.forName("android.os.ServiceManager").getMethod("getService", String.class).invoke(null, "clipboard");
        service = Class.forName("android.content.IClipboard$Stub").getMethod("asInterface", IBinder.class).invoke(null, binder);
        Class<?> type = Class.forName("android.content.IOnPrimaryClipChangedListener");
        Binder callback = new Binder() {
            @Override protected boolean onTransact(int code, Parcel data, Parcel reply, int flags) {
                if (code == INTERFACE_TRANSACTION) { if (reply != null) reply.writeString("android.content.IOnPrimaryClipChangedListener"); return true; }
                if (code != FIRST_CALL_TRANSACTION) return false;
                data.enforceInterface("android.content.IOnPrimaryClipChangedListener");
                synchronized (ShellClipboard.this) { revision++; }
                Bundle b = PrivilegeWire.result("clipboard_changed"); b.putLong("revision", revision());
                changed.accept(b); return true;
            }
        };
        listener = Proxy.newProxyInstance(type.getClassLoader(), new Class<?>[]{type}, (p,m,a) -> {
            if (m.getName().equals("asBinder")) return callback;
            throw new UnsupportedOperationException(m.getName());
        });
    }
    synchronized long revision() { return revision; }
    synchronized void start() throws Exception {
        if (!registered) { invoke("addPrimaryClipChangedListener", listener); registered = true; }
    }
    synchronized Bundle read() throws Exception {
        ClipData clip = (ClipData) invoke("getPrimaryClip", null);
        Bundle b = PrivilegeWire.result("unreadable"); b.putLong("revision", revision);
        // A locked-profile null is not an empty clipboard and must never clear remote state.
        if (clip == null || clip.getItemCount() != 1 || clip.getItemAt(0).getText() == null) return b;
        String text = clip.getItemAt(0).getText().toString();
        if (text.getBytes(StandardCharsets.UTF_8).length > PrivilegeWire.MAX_TEXT_BYTES) return PrivilegeWire.result("too_large");
        if (Build.VERSION.SDK_INT >= 33 && clip.getDescription().getExtras() != null)
            b.putBoolean("sensitive", clip.getDescription().getExtras().getBoolean(android.content.ClipDescription.EXTRA_IS_SENSITIVE));
        byte[] marker = ClipMarker.read(clip.getDescription());
        if (marker != null) b.putByteArray("marker", marker);
        // The clip's rich text next to its plain text (user 2026-11-12: every format is sent).
        String html = clip.getItemAt(0).getHtmlText();
        if (html != null && !html.isEmpty() && html.getBytes(StandardCharsets.UTF_8).length <= PrivilegeWire.MAX_HTML_BYTES)
            b.putString("html", html);
        b.putString("status", "text"); b.putString("text", text); return b;
    }
    /** {@code html}: the text's rich text, written as one HTML clip (text/plain + text/html) like the app's own writes. */
    synchronized Bundle write(String text, String html, long expectedRevision, boolean sensitive, byte[] marker) throws Exception {
        if(!registered) return PrivilegeWire.result("owner_inactive");
        if (expectedRevision != revision) return PrivilegeWire.result("stale");
        if (text == null || text.getBytes(StandardCharsets.UTF_8).length > PrivilegeWire.MAX_TEXT_BYTES) return PrivilegeWire.result("too_large");
        if (html != null && html.getBytes(StandardCharsets.UTF_8).length > PrivilegeWire.MAX_HTML_BYTES) return PrivilegeWire.result("too_large");
        ClipData clip = html == null || html.isEmpty() ? ClipData.newPlainText("Oink", text) : ClipData.newHtmlText("Oink", text, html);
        ClipMarker.describe(clip, marker, sensitive);
        invoke("setPrimaryClip", clip);
        return PrivilegeWire.result("written");
    }
    // Hidden IClipboard adds attribution/device parameters across releases. Only these known
    // methods are selected; shell package + primary user/device are fixed, never caller supplied.
    // API35 tested. Other API levels remain experimental until the device matrix passes.
    private Object invoke(String name, Object first) throws Exception {
        for (Method m : service.getClass().getMethods()) {
            if (!m.getName().equals(name)) continue;
            Class<?>[] types = m.getParameterTypes(); Object[] args = new Object[types.length];
            int strings = 0;
            for (int i=0;i<types.length;i++) {
                if (types[i] == String.class) args[i] = strings++ == 0 ? "com.android.shell" : null;
                else if (types[i] == int.class) args[i] = 0;
                else if (i == 0 && first != null && types[i].isInstance(first)) args[i] = first;
                else throw new UnsupportedOperationException("clipboard signature");
            }
            return m.invoke(service, args);
        }
        throw new NoSuchMethodException(name);
    }
    @Override public synchronized void close() throws Exception {
        if (registered) { invoke("removePrimaryClipChangedListener", listener); registered = false; }
    }
}
