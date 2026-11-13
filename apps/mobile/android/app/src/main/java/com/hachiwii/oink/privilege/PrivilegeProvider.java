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
        if ("bootstrap_identity".equals(method)) return bootstrapIdentity(arg);
        if ("bootstrap_disable".equals(method)) {
            // Persist first so a simultaneous boot/network retry cannot revive
            // a helper after PC disable. RecoveryService owns retry cancellation.
            Context context = getContext();
            boolean disabled = arg != null && com.hachiwii.oink.recovery.RecoveryController.INSTANCE.compareAndDisable(context, arg);
            Bundle result = new Bundle();
            result.putBoolean("disabled", disabled);
            return result;
        }
        if (!"attach".equals(method) || extras == null || extras.getInt("version") != 1)
            throw new IllegalArgumentException("unsupported bootstrap");
        IBinder helper = extras.getBinder("helper");
        if (helper == null || extras.getString("incarnation") == null) throw new IllegalArgumentException("missing helper");
        try { return PrivilegeClient.attach(helper, extras.getString("incarnation"), extras.getString("nonce")); }
        catch (RemoteException e) { throw new IllegalStateException(e); }
    }
    private Bundle bootstrapIdentity(String nonce) {
        Context context = getContext();
        Bundle result = new Bundle();
        // Recovery intent is DE. Identity remains owned by Rust in CE and must
        // never cause repository initialization while the user is locked.
        com.hachiwii.oink.recovery.RecoveryIntent intent = new com.hachiwii.oink.recovery.RecoveryStore(context).intentSnapshot();
        boolean enabled = intent.getEnabled();
        result.putBoolean("privilege_enabled", enabled);
        result.putString("intent_revision", intent.getRevision());
        UserManager users = context.getSystemService(UserManager.class);
        if (users == null || !users.isUserUnlocked()) return result;
        com.hachiwii.oink.OinkApplication app =
            (com.hachiwii.oink.OinkApplication) context.getApplicationContext();
        String identity = app.getRepository().bootstrapDeviceId();
        result.putBoolean("owner_running", identity != null);
        // Reuse the recovery store's existing public identity only after unlock.
        // No key or second identity cache is created by this provider.
        if (identity == null) identity = context.createDeviceProtectedStorageContext()
            .getSharedPreferences("privilege_recovery", Context.MODE_PRIVATE).getString("local", null);
        if (identity == null || !identity.matches("[0-9a-fA-F]{64}")) return result;
        result.putString("device_id", identity);
        try {
            result.putString("helper_version", context.getPackageManager()
                .getPackageInfo(context.getPackageName(), 0).versionName);
        } catch (android.content.pm.PackageManager.NameNotFoundException error) {
            throw new IllegalStateException("Package version unavailable", error);
        }
        try (java.io.InputStream source = new java.io.FileInputStream(context.getApplicationInfo().sourceDir)) {
            java.security.MessageDigest digest = java.security.MessageDigest.getInstance("SHA-256");
            byte[] buffer = new byte[32768];
            int count;
            while ((count = source.read(buffer)) != -1) digest.update(buffer, 0, count);
            StringBuilder hex = new StringBuilder(64);
            for (byte value : digest.digest()) hex.append(String.format(java.util.Locale.ROOT, "%02x", value & 255));
            result.putString("helper_digest", hex.toString());
        } catch (java.io.IOException | java.security.NoSuchAlgorithmException error) {
            throw new IllegalStateException("APK digest unavailable", error);
        }
        if (nonce != null && nonce.matches("[0-9a-f]{64}") && result.getBoolean("owner_running")) {
            String serial = android.provider.Settings.Secure.getString(context.getContentResolver(), android.provider.Settings.Secure.ANDROID_ID);
            if (serial == null || serial.isEmpty()) return result;
            StringBuilder encoded = new StringBuilder();
            for (byte value : serial.getBytes(java.nio.charset.StandardCharsets.UTF_8)) encoded.append(String.format(java.util.Locale.ROOT, "%02x", value & 255));
            result.putString("serial_hex", encoded.toString());
            String signature = app.getRepository().bootstrapIdentityProof(nonce, serial, result.getString("helper_digest"), enabled, intent.getRevision());
            if (signature != null) result.putString("signature", signature);
        }
        return result;
    }
    @Override public Cursor query(Uri u,String[] p,String s,String[] a,String o) { throw new UnsupportedOperationException(); }
    @Override public String getType(Uri u) { return null; }
    @Override public Uri insert(Uri u,ContentValues v) { throw new UnsupportedOperationException(); }
    @Override public int delete(Uri u,String s,String[] a) { throw new UnsupportedOperationException(); }
    @Override public int update(Uri u,ContentValues v,String s,String[] a) { throw new UnsupportedOperationException(); }
}
