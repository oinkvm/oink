//! The router's owner is transferred to the native capture thread. Both native input
//! callbacks and queue/timer wakeups enter it on that one thread; there is no shared lock.
use crate::{CapturedEvent, Disposition};
use std::{cell::RefCell, sync::Arc, time::Instant};

pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// The only global shortcut registered while sharing is paused. Native hotkey
/// APIs need one ordinary key and a modifier mask, never an input tap or hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResumeHotkey {
    pub key: crate::KeyCode,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

pub trait CaptureLoop: Send {
    /// Called on the capture thread before startup is reported complete.
    fn set_wake(&mut self, wake: Wake);
    fn event(&mut self, event: &CapturedEvent) -> Disposition;
    /// Observe external local keys solely for the Disabled release chord.
    fn injected_disable_release(&mut self, _code: crate::KeyCode, _pressed: bool) {}
    /// Retire an observed local replacement UP only when its source press is
    /// still exclusively Local. The native owner attests a physical first DOWN
    /// passed locally and an UP not rejected by the downstream chain.
    /// Bookkeeping only: queue native outputs for the next poll, never dispatch
    /// native work here (the caller must finish retiring the same native press).
    fn release_local_key(&mut self, _event: &CapturedEvent) -> bool {
        false
    }
    /// Local source gate; receiving remote input remains independent.
    fn capture_available(&mut self, _available: bool) {}
    /// Capture was intentionally withdrawn (permission/safety/pause), not a
    /// failed native attachment. Existing owners still receive source withdrawal.
    fn capture_detached(&mut self) {
        self.capture_available(false);
    }
    /// Drain ready work and return the next motion/router deadline.
    fn poll(&mut self, now: Instant) -> Option<Instant>;
    fn stopped(&self) -> bool;
    fn trace_focus(&self) -> Option<(oink_foundation::DeviceId, u64)> {
        None
    }
    /// The event loop remains alive when platform capture is withdrawn.
    fn local_rewrite(&self) -> Option<oink_foundation::input::LocalRewrite> {
        None
    }
    fn local_modifier_mapping(&self) -> (Vec<(crate::KeyCode, crate::KeyCode)>, Vec<crate::KeyCode>) {
        (Vec::new(), Vec::new())
    }
    /// Live pointer authority, independently of keyboard-local preferences.
    fn pointer_holder(&self) -> bool {
        false
    }
    fn keyboard_remote(&self) -> Option<bool> {
        None
    }
    /// Source mouse guard includes untranslated native pointer/gesture events.
    fn mouse_disabled(&self) -> bool {
        false
    }
    fn keyboard_disabled(&self) -> bool {
        false
    }
    /// Windows-local preference, enabled by default. Other backends retain their source policy.
    fn forward_injected_input(&self) -> bool {
        true
    }
    /// Guard-specific pass-through for the Windows capturable lock curtain.
    fn keyboard_guard_suspended(&self) -> bool {
        false
    }
    fn keyboard_pointer_modifiers(&self) -> Vec<crate::KeyCode> {
        Vec::new()
    }
    fn keyboard_key_blocked(&self, _code: Option<crate::KeyCode>) -> bool {
        self.keyboard_disabled()
    }
    /// Whether input-driven session idle refresh is currently permitted.
    fn idle_refresh_allowed(&self) -> bool {
        false
    }
    fn capture_requested(&self) -> bool {
        true
    }
    fn paused_hotkey(&self) -> Option<ResumeHotkey> {
        None
    }
    fn resume_hotkey(&mut self) {}
}

thread_local! { static OWNER: RefCell<Option<Box<dyn CaptureLoop>>> = const { RefCell::new(None) }; }
pub(crate) fn install(owner: Box<dyn CaptureLoop>, wake: Wake) {
    let mut owner = owner;
    owner.set_wake(wake);
    OWNER.with(|slot| *slot.borrow_mut() = Some(owner));
}
pub(crate) fn capture_available(available: bool) {
    OWNER.with(|slot| {
        if let Some(owner) = slot.borrow_mut().as_mut() {
            owner.capture_available(available);
        }
    });
}
#[cfg(any(target_os = "macos", windows))]
pub(crate) fn capture_detached() {
    OWNER.with(|slot| {
        if let Some(owner) = slot.borrow_mut().as_mut() {
            owner.capture_detached();
        }
    });
}
pub(crate) fn clear() {
    OWNER.with(|slot| slot.borrow_mut().take());
}
pub(crate) fn event(event: &CapturedEvent) -> Disposition {
    OWNER.with(|slot| {
        // Windows: SendInput issued from this hook thread while event()/poll()
        // owns the router (cursor placement, mode restore) runs the LL hooks
        // synchronously on the same thread before SendInput returns, and they
        // may carry other pending physical input (Sentry RUST-6). The router
        // is mid-operation: the nested event passes unrouted, like every
        // other reentrant hook callback.
        let Ok(mut owner) = slot.try_borrow_mut() else {
            let n = REENTERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!(n, "capture event nested in its busy owner passed unrouted");
            }
            return Disposition::Pass;
        };
        owner.as_mut().expect("capture owner installed").event(event)
    })
}
static REENTERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) fn poll(now: Instant) -> (Option<Instant>, bool) {
    OWNER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let owner = slot.as_mut().expect("capture owner installed");
        (owner.poll(now), owner.stopped())
    })
}

pub(crate) fn capture_requested() -> bool {
    OWNER.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("capture owner installed")
            .capture_requested()
    })
}

pub(crate) fn paused_hotkey() -> Option<ResumeHotkey> {
    OWNER.with(|slot| slot.borrow().as_ref().and_then(|owner| owner.paused_hotkey()))
}
pub(crate) fn resume_hotkey() {
    OWNER.with(|slot| {
        if let Some(owner) = slot.borrow_mut().as_mut() {
            owner.resume_hotkey();
        }
    });
}

#[cfg(windows)]
pub(crate) fn keyboard_remote() -> Option<bool> {
    OWNER.with(|slot| slot.try_borrow().ok()?.as_ref()?.keyboard_remote())
}

#[cfg(windows)]
pub(crate) fn callback_available() -> bool {
    // A nested native message may arrive while event()/poll() owns the router.
    // It must defer its release instead of borrowing that owner a second time.
    OWNER.with(|slot| slot.try_borrow_mut().is_ok())
}

#[cfg(windows)]
pub(crate) fn injected_disable_release(code: crate::KeyCode, pressed: bool) {
    OWNER.with(|slot| {
        if let Ok(mut owner) = slot.try_borrow_mut()
            && let Some(owner) = owner.as_mut()
        {
            owner.injected_disable_release(code, pressed);
        }
    });
}
thread_local! { static MOUSE_DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
/// The hook reads the guard for every physical mouse event, so the value kept
/// here is the owner's answer for the previous event on this thread. A
/// callback nested in the busy owner (see `event`) uses it: the guard is a
/// user setting, unchanged by the routing work that caused the nesting.
pub(crate) fn mouse_disabled() -> bool {
    OWNER.with(|slot| match slot.try_borrow() {
        Ok(owner) => {
            let disabled = owner.as_ref().is_some_and(|owner| owner.mouse_disabled());
            MOUSE_DISABLED.set(disabled);
            disabled
        }
        Err(_) => MOUSE_DISABLED.get(),
    })
}
pub(crate) fn keyboard_disabled() -> bool {
    OWNER.with(|slot| slot.borrow().as_ref().is_some_and(|owner| owner.keyboard_disabled()))
}

#[cfg(windows)]
pub(crate) fn forward_injected_input() -> Option<bool> {
    // A downstream hook may pump a native callback while the owner is borrowed.
    // The hook keeps the most recent requested value for that reentry only.
    OWNER.with(|slot| {
        slot.try_borrow()
            .ok()
            .map(|owner| owner.as_ref().is_none_or(|owner| owner.forward_injected_input()))
    })
}

#[cfg(windows)]
pub(crate) fn keyboard_guard_suspended() -> bool {
    OWNER.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|owner| owner.keyboard_guard_suspended())
    })
}

pub(crate) fn keyboard_key_blocked(code: Option<crate::KeyCode>) -> bool {
    OWNER.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|owner| owner.keyboard_key_blocked(code))
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn keyboard_pointer_modifiers() -> Vec<crate::KeyCode> {
    OWNER.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or_else(Vec::new, |owner| owner.keyboard_pointer_modifiers())
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn local_rewrite() -> Option<oink_foundation::input::LocalRewrite> {
    OWNER.with(|slot| slot.borrow().as_ref().and_then(|owner| owner.local_rewrite()))
}
#[cfg(target_os = "macos")]
pub(crate) fn local_modifier_mapping() -> (Vec<(crate::KeyCode, crate::KeyCode)>, Vec<crate::KeyCode>) {
    OWNER.with(|slot| {
        slot.try_borrow()
            .ok()
            .and_then(|slot| slot.as_ref().map(|owner| owner.local_modifier_mapping()))
            .unwrap_or_default()
    })
}

// A nested native callback may run while routing is borrowed; report unknown.
pub(crate) fn trace_focus() -> Option<(oink_foundation::DeviceId, u64)> {
    OWNER.with(|slot| slot.try_borrow().ok()?.as_ref()?.trace_focus())
}

#[cfg(target_os = "macos")]
pub(crate) fn idle_refresh_allowed() -> bool {
    OWNER.with(|slot| slot.borrow().as_ref().is_some_and(|owner| owner.idle_refresh_allowed()))
}

#[cfg(windows)]
pub(crate) fn pointer_holder() -> bool {
    // A borrowed owner is unknown, never authority to reset native visibility.
    // Standalone backends have no router; their Local mode is their authority.
    OWNER.with(|slot| {
        slot.try_borrow()
            .is_ok_and(|slot| slot.as_ref().is_none_or(|owner| owner.pointer_holder()))
    })
}

#[cfg(windows)]
pub(crate) fn release_local_key(event: &CapturedEvent) -> bool {
    OWNER.with(|slot| {
        slot.try_borrow_mut()
            .ok()
            .and_then(|mut slot| slot.as_mut().map(|owner| owner.release_local_key(event)))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CapturedEvent, InputEvent, SourceKind};
    use std::{
        cell::Cell,
        sync::atomic::{AtomicUsize, Ordering},
    };

    thread_local! { static NESTED: Cell<Option<(bool, Disposition)>> = const { Cell::new(None) }; }

    /// Mirrors RUST-6: work done while event()/poll() owns the router (on
    /// Windows, SendInput from the hook thread) synchronously delivers another
    /// native callback, which asks the owner for its mouse guard and routes the
    /// nested event on the same thread.
    struct Busy {
        disabled: bool,
        events: Arc<AtomicUsize>,
    }
    impl CaptureLoop for Busy {
        fn set_wake(&mut self, _: Wake) {}
        fn event(&mut self, _: &CapturedEvent) -> Disposition {
            self.events.fetch_add(1, Ordering::Relaxed);
            Disposition::Swallow
        }
        fn poll(&mut self, _: Instant) -> Option<Instant> {
            let event = CapturedEvent {
                event: InputEvent::PointerDelta { dx: 1.0, dy: 0.0 },
                physical: true,
                source: SourceKind::Mouse,
                time_us: 0,
                cursor: None,
            };
            NESTED.set(Some((super::mouse_disabled(), super::event(&event))));
            None
        }
        fn stopped(&self) -> bool {
            false
        }
        fn mouse_disabled(&self) -> bool {
            self.disabled
        }
    }

    #[test]
    fn a_callback_nested_in_the_owner_reads_the_guard_and_is_not_routed_twice() {
        for disabled in [false, true] {
            let events = Arc::new(AtomicUsize::new(0));
            install(
                Box::new(Busy {
                    disabled,
                    events: events.clone(),
                }),
                Arc::new(|| {}),
            );
            // The hook reads the guard for every physical event before routing.
            assert_eq!(mouse_disabled(), disabled);
            NESTED.set(None);
            poll(Instant::now());
            assert_eq!(NESTED.get(), Some((disabled, Disposition::Pass)), "disabled={disabled}");
            assert_eq!(
                events.load(Ordering::Relaxed),
                0,
                "the busy router never sees the nested event"
            );
            clear();
        }
    }
}
