package com.hachiwii.oink.privilege;

import android.os.*;


/** App-side seam: the Rust owner installs one Listener and enables the session explicitly. */
public final class PrivilegeClient {
    public interface Listener { void event(Bundle event); }
    private static IBinder helper;
    private static String nonce;
    private static String incarnation;
    private static Listener listener;
    private static boolean enabled;
    private PrivilegeClient() {}
    public static synchronized void setListener(Listener value) { listener = value; }
    public static synchronized boolean available() { return helper != null && helper.isBinderAlive(); }
    static synchronized Bundle attach(IBinder remote, String instance, String token) throws RemoteException {
        if (helper != null && helper.isBinderAlive()) throw new IllegalStateException("helper already attached");
        if(token==null)throw new IllegalArgumentException("nonce required");
        IBinder callback = new Binder() {
            @Override protected boolean onTransact(int code, Parcel data, Parcel reply, int flags) {
                if (code != PrivilegeWire.CALL) return false;
                if (Binder.getCallingUid() != 2000) throw new SecurityException("shell required");
                data.enforceInterface(PrivilegeWire.DESCRIPTOR);
                Bundle event = data.readBundle(getClass().getClassLoader());
                Listener target;
                synchronized (PrivilegeClient.class) {
                    if (event == null || !token.equals(event.getString("nonce")) || !instance.equals(event.getString("incarnation")) || helper != remote) throw new SecurityException("stale session");
                    target = enabled ? listener : null;
                }
                if (target != null) {
                    try { target.event(event); }
                    catch(RuntimeException e) { android.util.Log.e("OinkPrivilege","listener failed",e); }
                }
                return true;
            }
        };
        remote.linkToDeath(() -> {
            Listener target;
            synchronized (PrivilegeClient.class) {
                if (helper != remote) return;
                helper = null; nonce = null; enabled = false; target = listener;
            }
            com.hachiwii.oink.recovery.RecoveryController.helperChanged();
            if (target != null) {
                try { Bundle lost=PrivilegeWire.result("ipc_lost"); lost.putString("incarnation", instance); target.event(lost); }
                catch(RuntimeException e) { android.util.Log.e("OinkPrivilege","listener failed",e); }
            }
        }, 0);
        Bundle bind = new Bundle();
        bind.putString("op", "bind_callback"); bind.putString("nonce", token);
        bind.putString("incarnation", instance); bind.putBinder("callback", callback);
        PrivilegeWire.call(remote, bind);
        helper = remote; nonce = token; incarnation = instance;
        com.hachiwii.oink.recovery.RecoveryController.helperChanged();
        Bundle b = PrivilegeWire.result("attached"); b.putString("nonce", token); b.putBinder("callback", callback); return b;
    }
    /** Call off the UI thread; keep input and clipboard on separate serial executors. */
    public static Bundle request(Bundle request) throws RemoteException {
        return request(request, null);
    }
    /** Queued native work binds the helper that produced its observation. */
    public static Bundle request(Bundle request, String expectedIncarnation) throws RemoteException {
        IBinder target;
        synchronized (PrivilegeClient.class) {
            if (helper == null) throw new RemoteException("helper unavailable");
            if (expectedIncarnation != null && !expectedIncarnation.equals(incarnation))
                throw new RemoteException("helper incarnation changed");
            target = helper; request = new Bundle(request);
            request.putString("nonce", nonce); request.putString("incarnation", incarnation);
        }
        Bundle reply = PrivilegeWire.call(target, request);
        synchronized (PrivilegeClient.class) {
            if (helper != target) throw new RemoteException("helper session retired");
        }
        return reply;
    }
    public static Bundle enable(boolean value) throws RemoteException { return enable(value, null); }
    public static synchronized Bundle enable(boolean value, String expectedIncarnation) throws RemoteException {
        if (expectedIncarnation != null && !expectedIncarnation.equals(incarnation))
            throw new RemoteException("helper incarnation changed");
        Bundle b = new Bundle(); b.putString("op", value ? "enable" : "disable");
        synchronized (PrivilegeClient.class) { enabled=value; }
        Bundle result;
        try { result=request(b, expectedIncarnation); }
        catch(RemoteException | RuntimeException e) {
            synchronized (PrivilegeClient.class) { enabled=false; }
            throw e;
        }
        synchronized (PrivilegeClient.class) { enabled = value && "ok".equals(result.getString("status")); }
        return result;
    }
}
