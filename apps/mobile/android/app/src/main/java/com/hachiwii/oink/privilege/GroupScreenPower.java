package com.hachiwii.oink.privilege;

import android.os.Handler;
import android.os.PowerManager;
import android.os.SystemClock;

/** Shell-owned power lease, independent of pointer focus and keyguard state. */
final class GroupScreenPower implements AutoCloseable {
    static final long FRESH_MS=3000;
    private final PowerManager power;
    private final Handler events;
    private final PowerManager.WakeLock bright;
    private long until;
    private final Runnable expired=this::refresh;

    @SuppressWarnings("deprecation")
    GroupScreenPower(PowerManager power,Handler events) {
        this.power=power;this.events=events;
        // Activity.FLAG_KEEP_SCREEN_ON only covers our foreground window, whereas
        // Oink controls other apps. Shell's bright lock works on Android 15/HyperOS;
        // PARTIAL_WAKE_LOCK does not hold the display. Never use CAUSES_WAKEUP or
        // ON_AFTER_RELEASE: explicit entry alone wakes; real group pulses set idle time.
        bright=power.newWakeLock(PowerManager.SCREEN_BRIGHT_WAKE_LOCK,"Oink:group-input");
        bright.setReferenceCounted(false);
    }
    synchronized void activity(long expiresElapsed) throws Exception {
        long now=SystemClock.elapsedRealtime();
        until=Math.min(expiresElapsed,now+FRESH_MS);
        if(until>now && power.isInteractive()) {
            // This is one authenticated real input pulse, not a timer-generated
            // activity. It leaves the normal OS timeout measured from the last input.
            PowerManager.class.getMethod("userActivity",long.class,int.class,int.class)
                .invoke(power,SystemClock.uptimeMillis(),0,0);
        }
        refresh();
    }
    synchronized void enter() throws Exception {
        // Called with cleared Binder identity (shell DEVICE_POWER), before UHID
        // readiness. The pending-key path still drops early characters, never replays
        // them into a newly shown password field. wakeUp preserves the keyguard.
        PowerManager.class.getMethod("wakeUp",long.class,int.class,String.class)
            .invoke(power,SystemClock.uptimeMillis(),2,"Oink:focus-enter");
        refresh();
    }
    synchronized void displayChanged() { refresh(); }
    private synchronized void refresh() {
        events.removeCallbacks(expired);
        long remaining=until-SystemClock.elapsedRealtime();
        if(remaining>0 && power.isInteractive()) {
            if(!bright.isHeld())bright.acquire();
        } else if(bright.isHeld())bright.release();
        // A dead App/Binder lane cannot latch the lock. The shell also expires it,
        // even if the Rust/Kotlin expiry callback is delayed or never arrives.
        if(remaining>0)events.postDelayed(expired,remaining);
    }
    @Override public synchronized void close() {
        until=0;refresh();
    }
}
