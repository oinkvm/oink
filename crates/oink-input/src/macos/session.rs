//! Session notifications are observation only: never change focus or replay keys.
//! Unknown until the first notification (in particular when starting already locked).
use std::ffi::c_void;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, CFType};
use objc2_core_graphics::CGSessionCopyCurrentDictionary;

static STATE: AtomicU64 = AtomicU64::new(0);
static SHIELDED: AtomicBool = AtomicBool::new(false);

/// Do not deliver idle input into a screen saver, password grace period, lock
/// screen or another console session. Query also covers starting already locked.
pub(crate) fn idle_refresh_allowed() -> bool {
    if SHIELDED.load(Ordering::Acquire) || locked() == Some(true) {
        return false;
    }
    let Some(dict) = CGSessionCopyCurrentDictionary() else {
        return false;
    };
    // SAFETY: CG's session dictionary has string keys and CF object values.
    let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
    let flag = |key| {
        dict.get(&CFString::from_static_str(key))
            .and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
    };
    flag("kCGSSessionOnConsoleKey") == Some(true)
        && flag("kCGSessionLoginDoneKey") == Some(true)
        && flag("CGSSessionScreenIsLocked") != Some(true)
}

pub(crate) fn snapshot() -> u64 {
    STATE.load(Ordering::Acquire)
}

pub(crate) fn locked() -> Option<bool> {
    let state = snapshot();
    (state != 0).then_some(state & 1 != 0)
}

fn next_state(old: u64, locked: bool) -> u64 {
    ((old & !1).wrapping_add(2)) | u64::from(locked)
}

fn changed(locked: bool) {
    let mut old = STATE.load(Ordering::Acquire);
    loop {
        match STATE.compare_exchange_weak(old, next_state(old, locked), Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(current) => old = current,
        }
    }
    tracing::info!(locked, "session lock notification");
}

extern "C" fn did_lock(_: *mut c_void, _: *mut c_void, _: *const c_void, _: *const c_void, _: *const c_void) {
    changed(true);
}
extern "C" fn did_unlock(_: *mut c_void, _: *mut c_void, _: *const c_void, _: *const c_void, _: *const c_void) {
    SHIELDED.store(false, Ordering::Release);
    changed(false);
}

extern "C" fn shield_on(_: *mut c_void, _: *mut c_void, _: *const c_void, _: *const c_void, _: *const c_void) {
    SHIELDED.store(true, Ordering::Release);
}

extern "C" fn shield_off(_: *mut c_void, _: *mut c_void, _: *const c_void, _: *const c_void, _: *const c_void) {
    SHIELDED.store(false, Ordering::Release);
}

type Callback = extern "C" fn(*mut c_void, *mut c_void, *const c_void, *const c_void, *const c_void);
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFNotificationCenterGetDistributedCenter() -> *mut c_void;
    fn CFNotificationCenterAddObserver(
        center: *mut c_void,
        observer: *const c_void,
        callback: Callback,
        name: *const c_void,
        object: *const c_void,
        suspension_behavior: isize,
    );
}

pub(crate) fn init() {
    static ONCE: Once = Once::new();
    static OBSERVER: u8 = 0;
    ONCE.call_once(|| {
        super::main_queue::run_async(|| {
            // SAFETY: process-lifetime observer, callbacks and distributed center. CF copies names.
            unsafe {
                let center = CFNotificationCenterGetDistributedCenter();
                if center.is_null() {
                    tracing::warn!("session notification center unavailable");
                    return;
                }
                for (name, callback) in [
                    ("com.apple.screenIsLocked", did_lock as Callback),
                    ("com.apple.screenIsUnlocked", did_unlock as Callback),
                    ("com.apple.screensaver.didstart", shield_on as Callback),
                    ("com.apple.screensaver.didstop", shield_off as Callback),
                    ("com.apple.shieldWindowRaised", shield_on as Callback),
                    ("com.apple.shieldWindowLowered", shield_off as Callback),
                ] {
                    let name = CFString::from_str(name);
                    CFNotificationCenterAddObserver(
                        center,
                        &raw const OBSERVER as *const c_void,
                        callback,
                        &*name as *const CFString as *const c_void,
                        std::ptr::null(),
                        4,
                    );
                }
            }
            tracing::info!("session lock observer installed; initial lock state unknown");
        });
    });
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn simulated_notifications_preserve_unknown_and_signal_each_transition() {
        let initial = 0;
        let lock = next_state(initial, true);
        let unlock = next_state(lock, false);
        let relock = next_state(unlock, true);
        assert_eq!(lock & 1, 1);
        assert_eq!(unlock & 1, 0);
        assert_eq!(relock & 1, 1);
        assert_ne!(lock, relock);
        assert_ne!(unlock, 0);
        assert_ne!(next_state(relock, true), relock);
    }
}
