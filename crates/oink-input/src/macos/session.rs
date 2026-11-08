//! Session observations never change focus or replay keys. Seed from the current
//! session dictionary at startup, then follow lock/unlock notifications.
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
    if !idle_refresh_unshielded() {
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

/// Synchronous authoritative sample, never called from an event-tap callback.
/// IOConsoleLocked is primary: on macOS 14.7.8 CG/notifications preceded the
/// actual password lock by 5.422 seconds (the configured grace period). Missing
/// or contradictory observations remain unknown; notifications cannot grant it.
pub(crate) fn authoritative() -> Option<bool> {
    type Cf = *const c_void;
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IORegistryGetRootEntry(port: u32) -> u32;
        fn IORegistryEntryCreateCFProperty(entry: u32, key: Cf, allocator: Cf, options: u32) -> Cf;
        fn IOObjectRelease(object: u32) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: Cf);
        fn CFGetTypeID(value: Cf) -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(value: Cf) -> u8;
    }
    let key = CFString::from_static_str("IOConsoleLocked");
    // SAFETY: read-only registry lookup, release both returned owned references;
    // only read a CFBoolean after validating its runtime type.
    let io = unsafe {
        let root = IORegistryGetRootEntry(0);
        if root == 0 {
            return None;
        }
        let value = IORegistryEntryCreateCFProperty(root, (&*key as *const CFString).cast(), std::ptr::null(), 0);
        IOObjectRelease(root);
        if value.is_null() {
            return None;
        }
        let flag = (CFGetTypeID(value) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(value) != 0);
        CFRelease(value);
        flag
    };
    let dict = CGSessionCopyCurrentDictionary()?;
    // SAFETY: CG's dictionary uses CFString keys and CF object values.
    let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
    let flag = |key| {
        dict.get(&CFString::from_static_str(key))
            .and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
    };
    authoritative_state(
        io,
        flag("CGSSessionScreenIsLocked"),
        flag("kCGSSessionOnConsoleKey"),
        flag("kCGSessionLoginDoneKey"),
    )
}
fn authoritative_state(io: Option<bool>, cg: Option<bool>, console: Option<bool>, login: Option<bool>) -> Option<bool> {
    if console != Some(true) || login != Some(true) {
        return None;
    }
    match (io, cg) {
        (Some(true), Some(true)) => Some(true),
        // The unlocked dictionary normally omits the lock key. IO must
        // positively confirm false; absence of CG's key alone is insufficient.
        (Some(false), None | Some(false)) => Some(false),
        _ => None,
    }
}

/// Callback-safe second gate. The ordinary request path already queried the
/// console/login dictionary; never perform that synchronous query inside a tap.
pub(crate) fn idle_refresh_unshielded() -> bool {
    idle_session_permitted(SHIELDED.load(Ordering::Acquire), locked())
}

fn idle_session_permitted(shielded: bool, locked: Option<bool>) -> bool {
    !shielded && locked != Some(true)
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

fn initial_lock_state(screen_locked: Option<bool>, on_console: Option<bool>, login_done: Option<bool>) -> Option<bool> {
    if screen_locked == Some(true) {
        Some(true)
    } else if on_console == Some(true) && login_done == Some(true) {
        // The unlocked dictionary can omit CGSSessionScreenIsLocked (observed
        // on macOS 26.6.2); absence alone says nothing about an off-console session.
        Some(false)
    } else {
        None
    }
}

fn initial_sample(state: &AtomicU64, locked: Option<bool>) {
    if let Some(locked) = locked {
        // A notification is newer evidence. Never replace it with a startup read.
        let _ = state.compare_exchange(0, next_state(0, locked), Ordering::AcqRel, Ordering::Acquire);
    }
}

fn sample_initial_lock_state() {
    let Some(dict) = CGSessionCopyCurrentDictionary() else {
        return;
    };
    // SAFETY: CG's session dictionary has string keys and CF object values.
    let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
    let flag = |key| {
        dict.get(&CFString::from_static_str(key))
            .and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
    };
    initial_sample(
        &STATE,
        initial_lock_state(
            flag("CGSSessionScreenIsLocked"),
            flag("kCGSSessionOnConsoleKey"),
            flag("kCGSessionLoginDoneKey"),
        ),
    );
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
                    sample_initial_lock_state();
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
            // Query on main after registering, including when the backend starts
            // behind loginwindow. No synchronous session query runs on injection.
            sample_initial_lock_state();
            tracing::info!(locked = ?locked(), "session lock observer installed");
        });
    });
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn authoritative_lock_requires_io_and_handles_unlocked_missing_cg_key() {
        assert_eq!(
            authoritative_state(Some(true), Some(true), Some(true), Some(true)),
            Some(true)
        );
        for cg in [None, Some(false)] {
            assert_eq!(
                authoritative_state(Some(false), cg, Some(true), Some(true)),
                Some(false)
            );
            assert_eq!(authoritative_state(Some(true), cg, Some(true), Some(true)), None);
        }
        assert_eq!(
            authoritative_state(Some(false), Some(true), Some(true), Some(true)),
            None
        );
        assert_eq!(authoritative_state(None, Some(true), Some(true), Some(true)), None);
        assert_eq!(
            authoritative_state(Some(true), Some(true), Some(false), Some(true)),
            None
        );
        assert_eq!(
            authoritative_state(Some(true), Some(true), Some(true), Some(false)),
            None
        );
    }
    #[test]
    fn startup_session_sample_handles_already_locked_and_unlocked_without_a_lock_key() {
        assert_eq!(initial_lock_state(Some(true), Some(true), Some(true)), Some(true));
        assert_eq!(initial_lock_state(Some(false), Some(true), Some(true)), Some(false));
        assert_eq!(initial_lock_state(None, Some(true), Some(true)), Some(false));
        assert_eq!(initial_lock_state(None, Some(false), Some(true)), None);
        assert_eq!(initial_lock_state(None, Some(true), Some(false)), None);
        assert_eq!(initial_lock_state(None, None, None), None);
        for locked in [false, true] {
            let state = AtomicU64::new(0);
            initial_sample(&state, None);
            assert_eq!(state.load(Ordering::Acquire), 0);
            initial_sample(&state, Some(locked));
            assert_eq!(state.load(Ordering::Acquire), next_state(0, locked));
            let notified = next_state(state.load(Ordering::Acquire), !locked);
            state.store(notified, Ordering::Release);
            initial_sample(&state, Some(locked));
            assert_eq!(state.load(Ordering::Acquire), notified);
        }
    }
    #[test]
    fn screensaver_grace_and_lock_block_idle_pulses() {
        assert!(idle_session_permitted(false, Some(false)));
        for locked in [None, Some(false), Some(true)] {
            assert!(!idle_session_permitted(true, locked));
        }
        assert!(!idle_session_permitted(false, Some(true)));
    }
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
