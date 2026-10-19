//! Content-free, one-second keyboard pipeline counters. A post is not an OS delivery receipt.
use objc2_core_graphics::{CGEventFlags, CGEventSource, CGEventSourceStateID};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static CAPTURED_LOCAL: AtomicU64 = AtomicU64::new(0);
static CAPTURED_REMOTE: AtomicU64 = AtomicU64::new(0);
static ECHO: AtomicU64 = AtomicU64::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static LEDGER_MODS: AtomicU64 = AtomicU64::new(0);
static LEDGER_KEYS: AtomicU64 = AtomicU64::new(0);
static PASSED: AtomicU64 = AtomicU64::new(0);
static SWALLOWED: AtomicU64 = AtomicU64::new(0);
static POST_CTRL_CMD: AtomicU64 = AtomicU64::new(0);
static POST_ALPHA: AtomicU64 = AtomicU64::new(0);
static BUTTON_FLAGS: AtomicU64 = AtomicU64::new(0);
static POST_FLAGS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn modifiers(flags: u64) -> u64 {
    flags
        & (CGEventFlags::MaskControl.0
            | CGEventFlags::MaskCommand.0
            | CGEventFlags::MaskShift.0
            | CGEventFlags::MaskAlternate.0
            | CGEventFlags::MaskSecondaryFn.0)
}
pub(crate) fn captured(own: bool, remote: bool) {
    let counter = if own {
        &ECHO
    } else if remote {
        &CAPTURED_REMOTE
    } else {
        &CAPTURED_LOCAL
    };
    counter.fetch_add(1, Relaxed);
}
pub(crate) fn injected(ok: bool, mods: u64, keys: usize) {
    REQUESTS.fetch_add(1, Relaxed);
    if !ok {
        FAILED.fetch_add(1, Relaxed);
    }
    LEDGER_MODS.store(modifiers(mods), Relaxed);
    LEDGER_KEYS.store(keys as u64, Relaxed);
}
pub(crate) fn disposition(swallow: bool) {
    (if swallow { &SWALLOWED } else { &PASSED }).fetch_add(1, Relaxed);
}
pub(crate) fn posted_flags(flags: u64, modifier: bool) {
    POST_ALPHA.store(u64::from(flags & CGEventFlags::MaskAlphaShift.0 != 0), Relaxed);
    POST_FLAGS.store(modifiers(flags), Relaxed);
    if !modifier && flags & (CGEventFlags::MaskControl.0 | CGEventFlags::MaskCommand.0) != 0 {
        POST_CTRL_CMD.fetch_add(1, Relaxed);
    }
}
pub(crate) fn button_flags(flags: u64) {
    BUTTON_FLAGS.store(modifiers(flags), Relaxed);
}
pub(crate) fn report(remote: bool, captured_held: usize, force: bool, tap_enabled: bool) {
    let local = CAPTURED_LOCAL.swap(0, Relaxed);
    let forwarded = CAPTURED_REMOTE.swap(0, Relaxed);
    let echo = ECHO.swap(0, Relaxed);
    let requests = REQUESTS.swap(0, Relaxed);
    let failed = FAILED.swap(0, Relaxed);
    let passed = PASSED.swap(0, Relaxed);
    let swallowed = SWALLOWED.swap(0, Relaxed);
    let nonmodifier_with_ctrl_cmd = POST_CTRL_CMD.swap(0, Relaxed);
    if force || local + forwarded + echo + requests + failed != 0 {
        let caps_lock = super::capslock::hid::caps_lock();
        tracing::debug!(caps_on = ?caps_lock,
            last_post_alpha = POST_ALPHA.load(Relaxed), remote, tap_enabled, passed, swallowed, nonmodifier_with_ctrl_cmd, locked = ?super::session::locked(),
            secure_input_cached = super::system::secure_input_active(),
            captured_local = local, captured_remote = forwarded, own_session_echo = echo,
            inject_requests = requests, inject_errors = failed, captured_held,
            injected_held = LEDGER_KEYS.load(Relaxed), injected_modifiers = LEDGER_MODS.load(Relaxed),
            last_post_modifiers = POST_FLAGS.load(Relaxed),
            last_button_modifiers = BUTTON_FLAGS.load(Relaxed),
            hid_modifiers = modifiers(CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState).0),
            session_modifiers = modifiers(CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState).0),
            "keyboard pipeline counts (post/echo do not prove password-field delivery)");
    }
}
