//! Windows "Quicker compatibility mode" (local preference, default off).
//!
//! Quicker 1.45.5 ignores mouse button events whose `dwExtraInfo` is non-zero
//! (it checks neither `LLMHF_INJECTED` nor integrity level; VM evidence, see
//! docs/design/windows-external-input.md §Quicker). With the mode on, Oink's
//! remote-input injection sends mouse button events with `dwExtraInfo = 0`
//! instead of the OINK tag, so its own hook can no longer recognise them by tag.
//! Keyboard injection keeps the tag (Quicker's key triggers ignore dwExtraInfo).
//!
//! Oink knows when it injects. Each remote mouse injection arms the mouse window
//! for [`WINDOW`] (monotonic clock). Inside it the mouse hook passes
//! flagged-injected, untagged, non-release mouse events straight to the next
//! hook without any Oink processing. Physical input and every button release
//! keep the ordinary path. No per-event matching.
//! Kept independent of Win32 so the policy is testable on every host.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long after an Oink injection flagged-injected input of the same kind is
/// treated as possibly Oink's own untagged event.
pub(crate) const WINDOW: Duration = Duration::from_secs(1);

static ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// One injection window. `0` means never armed; otherwise the armed instant as
/// nanoseconds after a process-wide monotonic base, plus one.
pub(crate) struct Window(AtomicU64);

fn base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

impl Window {
    pub(crate) const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub(crate) fn arm(&self, now: Instant) {
        // Callers pass Instant::now(); an instant before the lazily created base
        // clamps to it (only tests construct such instants).
        let at = now.saturating_duration_since(base()).as_nanos() as u64 + 1;
        // Concurrent injectors may race; keep the latest instant.
        self.0.fetch_max(at, Ordering::AcqRel);
    }

    #[cfg(all(test, debug_assertions))]
    pub(crate) fn clear(&self) {
        self.0.store(0, Ordering::Release);
    }

    pub(crate) fn active(&self, now: Instant) -> bool {
        let armed = self.0.load(Ordering::Acquire);
        if armed == 0 {
            return false;
        }
        let armed = base() + Duration::from_nanos(armed - 1);
        // An injector on another thread may arm after the hook read `now`.
        now.saturating_duration_since(armed) < WINDOW
    }
}

pub(crate) static MOUSE: Window = Window::new();

/// Whether the hook must hand this event straight to the next hook.
///
/// `own` covers Oink's tagged events and transition markers: they keep their
/// existing path (markers commit mode transitions). Releases are never skipped,
/// so a press passed or routed before the window always closes in Oink's held
/// ledgers (the WeType replacement-UP pairing depends on the same rule for keys).
pub(crate) fn skip(window_active: bool, injected: bool, own: bool, release: bool) -> bool {
    window_active && injected && !own && !release
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn window_opens_for_one_second_after_each_injection() {
        let mouse = Window::new();
        let t0 = base() + Duration::from_secs(5); // instants before the base clamp to it
        assert!(!mouse.active(t0));
        mouse.arm(t0);
        assert!(mouse.active(t0));
        assert!(mouse.active(t0 + WINDOW - Duration::from_millis(1)));
        assert!(!mouse.active(t0 + WINDOW));
        assert!(!Window::new().active(t0), "windows are per instance");
        // Every injection refreshes the window.
        mouse.arm(t0 + Duration::from_millis(900));
        assert!(mouse.active(t0 + Duration::from_millis(1800)));
        assert!(!mouse.active(t0 + Duration::from_millis(1900)));
    }

    #[test]
    fn a_hook_sample_taken_before_a_concurrent_arm_is_inside_the_window() {
        let window = Window::new();
        let t0 = base() + Duration::from_secs(5); // instants before the base clamp to it
        window.arm(t0 + Duration::from_millis(5));
        assert!(window.active(t0));
        // An older arm from a slower injector never shortens the window.
        window.arm(t0);
        assert!(window.active(t0 + Duration::from_millis(1004)));
    }

    #[test]
    fn only_flagged_injected_non_release_foreign_events_are_skipped() {
        for active in [false, true] {
            for injected in [false, true] {
                for own in [false, true] {
                    for release in [false, true] {
                        assert_eq!(
                            skip(active, injected, own, release),
                            active && injected && !own && !release,
                            "active={active} injected={injected} own={own} release={release}"
                        );
                    }
                }
            }
        }
        // Physical input is never affected; releases always reach the ledgers.
        assert!(!skip(true, false, false, false));
        assert!(!skip(true, true, false, true));
    }
}
