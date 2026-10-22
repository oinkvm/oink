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
    fn keyboard_remote(&self) -> Option<bool> {
        None
    }
    /// Source mouse guard includes untranslated native pointer/gesture events.
    fn constrained_cursor(&self) -> Option<oink_foundation::Point> {
        None
    }
    fn mouse_disabled(&self) -> bool {
        false
    }
    fn keyboard_disabled(&self) -> bool {
        false
    }
    /// Windows-local opt-in. Other backends retain their existing source policy.
    fn forward_injected_input(&self) -> bool {
        false
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
#[cfg(target_os = "macos")]
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
        slot.borrow_mut()
            .as_mut()
            .expect("capture owner installed")
            .event(event)
    })
}
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

#[cfg(target_os = "macos")]
pub(crate) fn constrained_cursor() -> Option<oink_foundation::Point> {
    OWNER.with(|slot| slot.borrow().as_ref().and_then(|owner| owner.constrained_cursor()))
}
pub(crate) fn mouse_disabled() -> bool {
    OWNER.with(|slot| slot.borrow().as_ref().is_some_and(|owner| owner.mouse_disabled()))
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
            .map(|owner| owner.as_ref().is_some_and(|owner| owner.forward_injected_input()))
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
