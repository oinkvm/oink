package com.hachiwii.oink.privilege;

import android.content.*;
import android.app.KeyguardManager;
import android.hardware.display.DisplayManager;
import android.graphics.Point;
import android.net.LocalServerSocket;
import android.os.*;
import android.view.*;
import android.system.Os;
import android.system.OsConstants;
import java.io.FileDescriptor;
import java.lang.reflect.InvocationTargetException;
import java.util.UUID;

/** Loaded from the installed APK by app_process, never a second network/Rust owner. */
public final class HelperMain {
    private final String incarnation = UUID.randomUUID().toString();
    private final int appUid;
    private final Context context;
    private final Handler events = new Handler(Looper.getMainLooper());
    private final UhidDevices uhid = new UhidDevices();
    private final ShellClipboard clipboard;
    private final GroupScreenPower screenPower;
    private volatile IBinder callback;
    private final String nonce = UUID.randomUUID().toString();
    private volatile boolean enabled;
    private OutputCapture outputCapture;
    private long outputGeneration;
    private IBinder outputCallback;
    private IBinder.DeathRecipient outputDeath;
    private volatile boolean uhidReady;
    private boolean keyboardFocused,keyboardDeviceReady,keyboardDropLogged;
    private int keyboardDeviceId=-1;
    private AbsoluteMouse drawnPointer;
    private boolean pointerAttempted;
    /** Why the last pointer attempt failed (the exception and its message); null when it worked or was not tried. */
    private String pointerFailure;
    private int displayWidth, displayHeight, displayRotation;
    @android.annotation.SuppressLint("MissingPermission")
    private void subscribeKeyguard() {
        if(Build.VERSION.SDK_INT<33)return;
        // Shell's WM listener works without an AMS ApplicationThread (API35).
        // This only refreshes ClipboardService's own accessibility facts. Keyguard
        // must not destroy input devices, revoke controllability, or reset focus.
        try {
            context.getSystemService(KeyguardManager.class).addKeyguardLockedStateListener(events::post,
                locked -> send(PrivilegeWire.result("clipboard_state_changed")));
        } catch(SecurityException e) { android.util.Log.w("OinkPrivilege","keyguard listener unavailable",e); }
    }
    // Use absolute SOURCE_MOUSE injection on every supported Android version. AOSP's
    // VirtualMouse applies display-wide velocity control and truncates fractional wheel
    // reports; shell has no per-device bypass. Feedback correction cannot undo acceleration.
    private void openPointer() {
        if(pointerAttempted || drawnPointer!=null)return;
        pointerAttempted=true;
        try {
            drawnPointer=new AbsoluteMouse(context);
            pointerFailure=null;
        } catch(Exception e) {
            closePointer();
            Throwable cause=e instanceof java.lang.reflect.InvocationTargetException && e.getCause()!=null ? e.getCause() : e;
            pointerFailure=cause.getClass().getSimpleName()+(cause.getMessage()==null?"":": "+cause.getMessage());
            android.util.Log.w("OinkPrivilege","pointer unavailable",e);
        }
    }
    private void closePointer() {
        try { if(drawnPointer!=null) drawnPointer.close(); } catch(Exception ignored) { }
        drawnPointer=null;
    }
    private volatile long displayGeneration=1;
    private boolean backHeld;
    private long touchDown;
    private float touchX, touchY;
    private boolean touching;
    private synchronized void releaseInput() throws Exception {
        try {
            uhid.releaseAll();
            if(drawnPointer!=null)drawnPointer.releaseAll();
        } finally {
            if(backHeld) { inject(new KeyEvent(KeyEvent.ACTION_UP,KeyEvent.KEYCODE_BACK)); backHeld=false; }
            if(touching) {
                MotionEvent cancel=MotionEvent.obtain(touchDown,SystemClock.uptimeMillis(),MotionEvent.ACTION_CANCEL,touchX,touchY,0);
                cancel.setSource(InputDevice.SOURCE_TOUCHSCREEN);
                try { inject(cancel); } finally { cancel.recycle(); touching=false; }
            }
        }
    }
    private final Runnable clipboardChanged = () -> {
        if (!enabled) return;
        Bundle b=PrivilegeWire.result("clipboard_changed"); b.putLong("revision", clipboardRevision()); send(b);
    };
    private long clipboardRevision() { return clipboard.revision(); }
    private HelperMain(Context context) throws Exception {
        this.context=context;
        appUid=context.getPackageManager().getApplicationInfo("com.hachiwii.oink",0).uid;
        clipboard=new ShellClipboard(b -> { events.removeCallbacks(clipboardChanged); events.post(clipboardChanged); });
        screenPower=new GroupScreenPower(context.getSystemService(PowerManager.class),events);
        updateDisplayGeometry();
        subscribeKeyguard();
        context.getSystemService(android.hardware.input.InputManager.class).registerInputDeviceListener(new android.hardware.input.InputManager.InputDeviceListener() {
            public void onInputDeviceChanged(int id) { }
            public void onInputDeviceRemoved(int id) {
                synchronized(HelperMain.this) {
                    if(id!=keyboardDeviceId)return;
                    keyboardDeviceReady=false;keyboardDeviceId=-1;
                }
                send(PrivilegeWire.result("keyboard_removed"));
            }
            public void onInputDeviceAdded(int id) {
                InputDevice d=InputDevice.getDevice(id);
                if(d==null || !"Oink keyboard".equals(d.getName()))return;
                synchronized(HelperMain.this) { if(!keyboardFocused)return;keyboardDeviceId=id;keyboardDeviceReady=true; }
                send(PrivilegeWire.result("keyboard_device_ready"));
            }
        },events);
        context.getSystemService(DisplayManager.class).registerDisplayListener(new DisplayManager.DisplayListener() {
            public void onDisplayAdded(int id) { changed(id); }
            public void onDisplayRemoved(int id) { changed(id); }
            public void onDisplayChanged(int id) { changed(id); }
            private void changed(int id) {
                if(id!=Display.DEFAULT_DISPLAY)return;
                boolean geometryChanged;
                synchronized(HelperMain.this) {
                    geometryChanged=updateDisplayGeometry();
                    if(enabled) {
                        screenPower.displayChanged();
                        if(geometryChanged)try { releaseInput(); } catch(Exception ignored) { }
                        openPointer();
                    }
                }
                if(enabled && geometryChanged)send(display());
            }
        },events);
    }
    private boolean updateDisplayGeometry() {
        Display display=context.getSystemService(DisplayManager.class).getDisplay(0);
        if(display==null)return false;
        Point size=new Point();display.getRealSize(size);
        int rotation=display.getRotation();
        if(size.x==displayWidth && size.y==displayHeight && rotation==displayRotation)return false;
        if(displayWidth!=0)displayGeneration++;
        displayWidth=size.x;displayHeight=size.y;displayRotation=rotation;
        return true;
    }
    private void send(Bundle b) {
        IBinder target=callback;
        if(target==null || nonce==null) return;
        b.putString("nonce",nonce); b.putString("incarnation",incarnation);
        try { PrivilegeWire.event(target,b); }
        catch(RemoteException e) {
            if(!target.isBinderAlive()) { disconnect(); System.exit(0); }
            else android.util.Log.w("OinkPrivilege","event delivery failed",e);
        }
    }
    private synchronized void disconnect() {
        stopOutputCapture();
        enabled=false; uhidReady=false; keyboardFocused=false; callback=null;
        screenPower.close();
        try { releaseInput(); } catch(Exception ignored) { }
        try { clipboard.close(); } catch(Exception ignored) { }
        try { uhid.close(); } catch(Exception ignored) { }
        closePointer();
    }
    private final Binder service = new Binder() {
        @Override protected boolean onTransact(int code, Parcel data, Parcel reply, int flags) {
            if (code == INTERFACE_TRANSACTION) { reply.writeString(PrivilegeWire.DESCRIPTOR); return true; }
            if (code != PrivilegeWire.CALL) return false;
            if(Binder.getCallingUid()!=appUid) throw new SecurityException("owner UID required");
            data.enforceInterface(PrivilegeWire.DESCRIPTOR);
            Bundle request=data.readBundle(getClass().getClassLoader());
            if(request==null || nonce==null || !nonce.equals(request.getString("nonce")) || !incarnation.equals(request.getString("incarnation"))) throw new SecurityException("stale session");
            // System service calls must retain the helper's shell identity, not the App caller.
            long identity=Binder.clearCallingIdentity();
            Bundle response;
            try { response=dispatch(request); }
            catch(Exception e) {
                Throwable cause=e instanceof InvocationTargetException ? e.getCause() : e;
                response=PrivilegeWire.result(cause instanceof SecurityException ? "permission_denied" : "failed");
                response.putString("error",cause.getClass().getSimpleName());
            } finally { Binder.restoreCallingIdentity(identity); }
            reply.writeNoException(); reply.writeBundle(response); return true;
        }
    };
    private synchronized Bundle display() {
        Bundle b=PrivilegeWire.result("display"); Display d=context.getSystemService(DisplayManager.class).getDisplay(0);
        if(d==null) return PrivilegeWire.result("display_unavailable");
        b.putInt("width",displayWidth); b.putInt("height",displayHeight);
        b.putInt("rotation",displayRotation); b.putInt("display_id",0); b.putLong("generation",displayGeneration); return b;
    }
    private Bundle dispatch(Bundle r) throws Exception {
        String op=r.getString("op", "");
        if(op.equals("clipboard_read") || op.equals("clipboard_write")) {
            if(!enabled) return PrivilegeWire.result("owner_inactive");
            return op.equals("clipboard_read") ? clipboard.read() : clipboard.write(r.getString("text"),r.getString("html"),r.getLong("revision",-1),r.getBoolean("sensitive"),r.getByteArray("marker"));
        }
        synchronized(this) { return control(r); }
    }
    private Bundle control(Bundle r) throws Exception {
        String op=r.getString("op", "");
        if(op.equals("bind_callback")) {
            if(callback!=null)throw new IllegalStateException("already bound");
            callback=r.getBinder("callback");
            if(callback==null)throw new IllegalArgumentException("callback required");
            return PrivilegeWire.result("ok");
        }
        if(op.equals("display")) return display();
        if(op.equals("health")) {
            Bundle b=PrivilegeWire.result(enabled ? "enabled" : "attached");
            b.putBoolean("keyboard_uhid",uhidReady); b.putBoolean("input_ready",enabled && uhidReady && (drawnPointer!=null));
            b.putString("mouse_path",drawnPointer!=null?"self_drawn_absolute":"unavailable");
            if(drawnPointer==null && pointerFailure!=null)b.putString("mouse_error",pointerFailure);
            b.putLong("output_capture_dropped_blocks", outputCapture == null ? 0 : outputCapture.droppedBlocks());
            b.putString("incarnation",incarnation); return b;
        }
        if(op.equals("stop")) {
            // "stop" is sent only when the user turns privileged mode off (RecoveryController).
            closePointer();
            if(Build.VERSION.SDK_INT>=35)VirtualPointer.removeAssociation(context);
            disconnect(); events.post(() -> System.exit(0)); return PrivilegeWire.result("stopping");
        }
        if(op.equals("enable")) {
            enabled=true;
            try { clipboard.start(); }
            catch(Exception failure) { enabled=false;throw failure; }
            try(java.io.FileOutputStream probe=new java.io.FileOutputStream("/dev/uhid")) { uhidReady=true; } catch(Exception e) { uhidReady=false; }
            pointerAttempted=false;
            openPointer();
            enabled=true; return PrivilegeWire.result("ok");
        }
        if(op.equals("disable")) {
            stopOutputCapture();
            enabled=false;keyboardFocused=false;
            screenPower.close();
            try { releaseInput(); }
            finally {
                try { clipboard.close(); }
                finally {
                    try { uhid.close(); }
                    finally {
                        closePointer();uhidReady=false;keyboardDeviceReady=false;keyboardDeviceId=-1;
                    }
                }
            }
            return PrivilegeWire.result("ok");
        }
        // Capture is independent of input/keyguard suspension. Stop is accepted even after disable.
        if(op.equals("stop_output_capture")) {
            if(r.getLong("generation") == outputGeneration) stopOutputCapture();
            return PrivilegeWire.result("ok");
        }
        if(op.equals("start_output_capture")) {
            if(enabled) return startOutputCapture(r);
            ParcelFileDescriptor pipe = r.getParcelable("pipe");
            if(pipe != null) pipe.close();
            return PrivilegeWire.result("owner_inactive");
        }
        if(!enabled) return PrivilegeWire.result("owner_inactive");
        if(op.equals("group_activity")) {
            screenPower.activity(r.getLong("until_elapsed_ms",0));
            return PrivilegeWire.result("ok");
        }
        if(op.equals("keyboard_focus")) {
                if(r.getBoolean("active")) {
                    // Only a new, validated FocusEnter wakes the phone. Group pulses and
                    // repeated focus commands never undo a user's explicit screen-off.
                    if(!keyboardFocused)screenPower.enter();
                    keyboardDropLogged=false;keyboardFocused=true;
                    uhid.openKeyboard();
                }
                else {
                    keyboardFocused=false;
                    try { uhid.releaseAll(); }
                    finally {
                        try { uhid.close(); }
                        finally { keyboardDeviceReady=false;keyboardDeviceId=-1;send(PrivilegeWire.result("keyboard_removed")); }
                    }
                }
            return PrivilegeWire.result("ok");
        }
        if(op.equals("mouse_absolute") || op.equals("sdk_touch")) {
            if(r.getLong("display_generation",-1)!=displayGeneration) { releaseInput(); return PrivilegeWire.result("stale_display"); }
        }
        switch(op) {
            case "release_all": releaseInput(); break;
            case "mouse_absolute":
                if(drawnPointer==null)return PrivilegeWire.result("pointer_unavailable");
                float px=r.getFloat("x"),py=r.getFloat("y");Bundle facts=display();
                if(px<0 || py<0 || px>=facts.getInt("width") || py>=facts.getInt("height"))return PrivilegeWire.result("outside_display");
                drawnPointer.update(px,py,r.getInt("buttons"),r.getFloat("wheel"),r.getFloat("hwheel"));break;
            case "system_gesture":
                if(r.getLong("display_generation",-1)!=displayGeneration) return PrivilegeWire.result("stale_display");
                if(!keyboardFocused) return PrivilegeWire.result("owner_inactive");
                SystemGestures.perform(r.getString("gesture")); break;
            case "keyboard":
                if(!keyboardFocused || !keyboardDeviceReady) {
                    if(!keyboardDropLogged) { android.util.Log.i("OinkPrivilege","keyboard dropped pending device");keyboardDropLogged=true; }
                    return PrivilegeWire.result("keyboard_pending");
                }
                uhid.keyboard(r.getInt("modifiers"),r.getByteArray("usages")); break;
            case "sdk_key": {
                int action=r.getInt("action"), key=r.getInt("key");
                if((action!=KeyEvent.ACTION_DOWN && action!=KeyEvent.ACTION_UP) || key!=KeyEvent.KEYCODE_BACK) return PrivilegeWire.result("unsupported_key");
                long now=SystemClock.uptimeMillis();
                backHeld=action==KeyEvent.ACTION_DOWN;
                return inject(new KeyEvent(now,now,action,key,0));
            }
            case "sdk_touch": {
                int action=r.getInt("action");
                if(action!=MotionEvent.ACTION_DOWN && action!=MotionEvent.ACTION_UP && action!=MotionEvent.ACTION_MOVE && action!=MotionEvent.ACTION_CANCEL) throw new IllegalArgumentException("touch action");
                float x=r.getFloat("x"), y=r.getFloat("y");
                if(!Float.isFinite(x) || !Float.isFinite(y) || x<0 || y<0) throw new IllegalArgumentException("touch coordinates");
                touchX=x; touchY=y; touching=action!=MotionEvent.ACTION_UP && action!=MotionEvent.ACTION_CANCEL;
                if(action==MotionEvent.ACTION_DOWN) touchDown=SystemClock.uptimeMillis();
                MotionEvent ev=MotionEvent.obtain(touchDown,SystemClock.uptimeMillis(),action,r.getFloat("x"),r.getFloat("y"),0);
                ev.setSource(InputDevice.SOURCE_TOUCHSCREEN);
                try { return inject(ev); } finally { ev.recycle(); }
            }
            default: return PrivilegeWire.result("unsupported");
        }
        return PrivilegeWire.result("ok");
    }
    private Bundle startOutputCapture(Bundle request) {
        ParcelFileDescriptor pipe = request.getParcelable("pipe");
        IBinder target = request.getBinder("capture_callback");
        long generation = request.getLong("generation");
        if (pipe == null || target == null) {
            if (pipe != null) try { pipe.close(); } catch (java.io.IOException ignored) { }
            return PrivilegeWire.result("invalid_capture");
        }
        if (outputCapture != null) {
            try { pipe.close(); } catch (java.io.IOException ignored) { }
            return PrivilegeWire.result("capture_busy");
        }
        outputGeneration = generation;
        outputCallback = target;
        outputDeath = () -> { synchronized (HelperMain.this) { if (outputCallback == target) stopOutputCapture(); } };
        try {
            target.linkToDeath(outputDeath, 0);
            outputCapture = OutputCapture.open(context, pipe, generation, event -> {
                event.putString("nonce", nonce); event.putString("incarnation", incarnation);
                try { PrivilegeWire.event(target, event); }
                catch (RemoteException e) { /* Binder death or pipe EPIPE closes the capture. */ }
            });
            return PrivilegeWire.result("ok");
        } catch (Exception e) {
            stopOutputCapture();
            try { pipe.close(); } catch (java.io.IOException ignored) { }
            Throwable cause = e instanceof InvocationTargetException ? e.getCause() : e;
            Bundle result = PrivilegeWire.result(cause instanceof SecurityException ? "capture_blocked" :
                cause instanceof OutputCapture.CaptureBusyException ? "capture_busy" : "capture_failed");
            result.putString("error", cause.getClass().getSimpleName());
            return result;
        }
    }
    private void stopOutputCapture() {
        if (outputCallback != null && outputDeath != null) outputCallback.unlinkToDeath(outputDeath, 0);
        outputCallback = null; outputDeath = null;
        if (outputCapture != null) { outputCapture.close(); outputCapture = null; }
    }
    private Bundle inject(InputEvent event) throws Exception {
        Object manager=Class.forName("android.hardware.input.InputManager").getMethod("getInstance").invoke(null);
        boolean accepted=(boolean)manager.getClass().getMethod("injectInputEvent",InputEvent.class,int.class).invoke(manager,event,0);
        return PrivilegeWire.result(accepted ? "injected" : "rejected");
    }
    public static void main(String[] args) throws Exception {
        if(android.os.Process.myUid()!=2000) throw new SecurityException("shell required");
        // systemMain initializes framework singletons. Without it InputManager terminated the
        // app_process probe on API35; plain Looper initialization was insufficient.
        Looper.prepareMainLooper();
        Context shell=ShellContext.create();
        if((int)Class.forName("android.app.ActivityManager").getMethod("getCurrentUser").invoke(null)!=0) throw new SecurityException("primary user required");
        // Abstract socket is only an exclusive process lock, never an unauthenticated command API.
        LocalServerSocket singleton=new LocalServerSocket("com.hachiwii.oink.privilege.v1");
        if(args.length==0) {
            // Detach from the ADB shell session as well as redirecting all three descriptors.
            // Bootstrap backgrounds app_process; '&' alone is not a lifecycle guarantee.
            Os.setsid();
            FileDescriptor devnull=Os.open("/dev/null",OsConstants.O_RDWR,0);
            try { Os.dup2(devnull,0); Os.dup2(devnull,1); Os.dup2(devnull,2); } finally { Os.close(devnull); }
        } else if(args.length!=1 || !"--foreground".equals(args[0])) throw new IllegalArgumentException("arguments");
        HelperMain helper=new HelperMain(shell);
        Bundle request=new Bundle(); request.putInt("version",1); request.putString("nonce",helper.nonce); request.putString("incarnation",helper.incarnation); request.putBinder("helper",helper.service);
        ShellBootstrap.attach(shell,request);

        helper.callback.linkToDeath(() -> { helper.disconnect(); System.exit(0); },0);
        Runtime.getRuntime().addShutdownHook(new Thread(helper::disconnect));
        Looper.loop(); singleton.close();
    }
}
