package com.hachiwii.oink.privilege;

import android.content.*;
import android.database.Cursor;
import android.net.Uri;
import android.os.*;

/** Bootstrap only; exported entry requires both DUMP permission and shell UID. */
public final class PrivilegeProvider extends ContentProvider {
    @Override public boolean onCreate() { return true; }
    @Override public Bundle call(String method, String arg, Bundle extras) {
        if (Binder.getCallingUid() != 2000) throw new SecurityException("shell required");
        if (!"attach".equals(method) || extras == null || extras.getInt("version") != 1)
            throw new IllegalArgumentException("unsupported bootstrap");
        IBinder helper = extras.getBinder("helper");
        if (helper == null || extras.getString("incarnation") == null) throw new IllegalArgumentException("missing helper");
        try { return PrivilegeClient.attach(helper, extras.getString("incarnation"), extras.getString("nonce")); }
        catch (RemoteException e) { throw new IllegalStateException(e); }
    }
    @Override public Cursor query(Uri u,String[] p,String s,String[] a,String o) { throw new UnsupportedOperationException(); }
    @Override public String getType(Uri u) { return null; }
    @Override public Uri insert(Uri u,ContentValues v) { throw new UnsupportedOperationException(); }
    @Override public int delete(Uri u,String s,String[] a) { throw new UnsupportedOperationException(); }
    @Override public int update(Uri u,ContentValues v,String s,String[] a) { throw new UnsupportedOperationException(); }
}
