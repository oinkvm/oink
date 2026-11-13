package com.hachiwii.oink.privilege;

import android.os.*;

/** Local-only Binder contract. Network messages remain owned by oink-protocol. */
public final class PrivilegeWire {
    public static final String DESCRIPTOR = "com.hachiwii.oink.privilege.v1";
    public static final int CALL = IBinder.FIRST_CALL_TRANSACTION;
    public static final int MAX_TEXT_BYTES = 65536;
    /** A clip's HTML travels with its text up to this size (UTF-8); larger HTML is left out and the text goes
     * alone. Bounds the Binder transaction (1 MB for the whole process) and the phone's automatic send. */
    public static final int MAX_HTML_BYTES = 256 * 1024;
    private PrivilegeWire() {}
    public static Bundle call(IBinder target, Bundle request) throws RemoteException {
        Parcel in = Parcel.obtain(), out = Parcel.obtain();
        try {
            in.writeInterfaceToken(DESCRIPTOR);
            in.writeBundle(request);
            if (!target.transact(CALL, in, out, 0)) throw new RemoteException("unsupported helper transaction");
            out.readException();
            return out.readBundle(PrivilegeWire.class.getClassLoader());
        } finally { in.recycle(); out.recycle(); }
    }
    public static void event(IBinder target, Bundle event) throws RemoteException {
        Parcel data = Parcel.obtain();
        try {
            data.writeInterfaceToken(DESCRIPTOR);
            data.writeBundle(event);
            if (!target.transact(CALL, data, null, IBinder.FLAG_ONEWAY)) {
                throw new RemoteException("unsupported event transaction");
            }
        } finally { data.recycle(); }
    }
    public static Bundle result(String status) {
        Bundle b = new Bundle(); b.putString("status", status); return b;
    }
}
