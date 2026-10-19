//! In-memory, observation-only context. Never part of captured/wire event serialization.
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawKeyEvent {
    #[default]
    Unknown,
    Down,
    Up,
    ModifierFlags,
    SystemFunction,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureMetadata {
    pub raw: RawKeyEvent,
    pub backend_generated: Option<bool>,
    pub repeat: Option<bool>,
    pub pressed_while_held: bool,
}
thread_local! {
    static CAPTURE: Cell<CaptureMetadata> = const { Cell::new(CaptureMetadata {
        raw: RawKeyEvent::Unknown, backend_generated: None, repeat: None, pressed_while_held: false,
    }) };
}
/// Valid only during the synchronous capture callback; contains no key identity.
pub fn capture_metadata() -> CaptureMetadata {
    CAPTURE.get()
}
#[cfg(any(target_os = "macos", windows))]
pub(crate) fn with_capture<T>(metadata: CaptureMetadata, f: impl FnOnce() -> T) -> T {
    struct Restore(CaptureMetadata);
    impl Drop for Restore {
        fn drop(&mut self) {
            CAPTURE.set(self.0);
        }
    }
    let _restore = Restore(CAPTURE.replace(metadata));
    f()
}

static CAPS: AtomicU8 = AtomicU8::new(0);
static SECURE: AtomicU8 = AtomicU8::new(0);
fn decode(value: u8) -> Option<bool> {
    match value {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}
/// Cache only: no OS calls, locks, names or input source IDs.
pub fn keyboard_state() -> [Option<bool>; 2] {
    [&CAPS, &SECURE].map(|v| decode(v.load(Ordering::Relaxed)))
}
#[cfg(target_os = "macos")]
fn changed(slot: &AtomicU8, value: bool) -> bool {
    slot.swap(1 + u8::from(value), Ordering::Relaxed) != 1 + u8::from(value)
}
#[cfg(target_os = "macos")]
pub(crate) fn caps(value: bool) {
    if changed(&CAPS, value) {
        tracing::info!(diagnostic_caps = value, "keyboard state");
    }
}
#[cfg(target_os = "macos")]
pub(crate) fn secure(value: bool) {
    if changed(&SECURE, value) {
        tracing::info!(diagnostic_secure = value, "keyboard state");
    }
}
// Observe the identity-free payload before tracing's process-global callsite
// filtering. Only the test's own thread enables this slot; other tests cannot
// add records to it, and production builds contain no observer.
#[cfg(all(debug_assertions, all(test, target_os = "macos")))]
thread_local! {
    pub(crate) static OBSERVED_NONMODIFIER_FLAGS: std::cell::RefCell<Option<Vec<bool>>> = const {
        std::cell::RefCell::new(None)
    };
}
#[cfg(target_os = "macos")]
pub(crate) fn nonmodifier_flags() {
    let nonmodifier_flags = true;
    #[cfg(all(test, debug_assertions))]
    OBSERVED_NONMODIFIER_FLAGS.with(|records| {
        if let Some(records) = records.borrow_mut().as_mut() {
            records.push(nonmodifier_flags);
        }
    });
    tracing::info!(nonmodifier_flags, "keyboard capture anomaly");
}

#[cfg(all(debug_assertions, all(test, target_os = "macos")))]
mod tests {
    use super::*;
    #[test]
    fn callback_context_is_nested_and_panic_safe() {
        let original = capture_metadata();
        let down = CaptureMetadata {
            raw: RawKeyEvent::Down,
            repeat: Some(true),
            ..Default::default()
        };
        with_capture(down, || {
            assert_eq!(capture_metadata(), down);
            let result = std::panic::catch_unwind(|| with_capture(CaptureMetadata::default(), || panic!("test")));
            assert!(result.is_err());
            assert_eq!(capture_metadata(), down);
        });
        assert_eq!(capture_metadata(), original);
    }
    #[test]
    fn state_notifications_only_fire_on_first_known_value_or_transition() {
        let state = AtomicU8::new(0);
        assert_eq!(decode(state.load(Ordering::Relaxed)), None);
        assert!(changed(&state, false));
        assert!(!changed(&state, false));
        assert!(changed(&state, true));
        assert!(!changed(&state, true));
        assert_eq!(decode(state.load(Ordering::Relaxed)), Some(true));
    }
}
