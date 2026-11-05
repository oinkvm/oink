//! Retarget-boundary validation, not a keyboard watchdog. Windows 11 26200
//! experiments show that swallowed DOWNs do not update async state or Raw Input.
//! Thus only a first physical DOWN passed locally may be compared with async
//! state. Never query inside a keyboard callback (state updates after LL hooks),
//! on an inaccessible desktop, or after synthetic input touched that identity.
use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

thread_local! {
    static KEYBOARD_DEPTH: Cell<u32> = const { Cell::new(0) };
    static KEYBOARD_REVISION: Cell<u64> = const { Cell::new(0) };
    // Must survive a synthetic callback reentering while State is borrowed.
    static INVALIDATED: Cell<u8> = const { Cell::new(0) };
}
const VKS: [i32; 8] = [0xa2, 0xa0, 0xa4, 0x5b, 0xa3, 0xa1, 0xa5, 0x5c];

pub(super) struct KeyboardCallback;
impl KeyboardCallback {
    pub(super) fn enter() -> Self {
        KEYBOARD_DEPTH.update(|n| n + 1);
        KEYBOARD_REVISION.update(|n| n.wrapping_add(1));
        Self
    }
}
impl Drop for KeyboardCallback {
    fn drop(&mut self) {
        KEYBOARD_DEPTH.update(|n| n - 1);
    }
}
pub(super) fn invalidate(vk: u32) {
    // SendInput/keybd_event accept neutral VK_SHIFT/CONTROL/MENU and
    // KBDLLHOOKSTRUCT does not promise a sided vkCode for them, while the edge
    // still lands on one side's async state. Ambiguous input taints both sides.
    let bit = match vk {
        0x10 => 0x22,
        0x11 => 0x11,
        0x12 => 0x44,
        _ => crate::windows_keyboard_trace::modifier(vk),
    };
    if bit != 0 {
        INVALIDATED.update(|mask| mask | bit);
        with_state(|st| st.passed_modifiers.invalidate(bit));
    }
}
pub(super) fn physical_entry(vk: u32, down: bool) {
    let bit = crate::windows_keyboard_trace::modifier(vk);
    if bit == 0 {
        return;
    }
    if !down {
        invalidate(vk);
    } else {
        let code = KeyCode::keyboard(0xe0 + bit.trailing_zeros() as u16);
        if with_state(|st| !st.keys.contains_key(&code.0)) == Some(true) {
            INVALIDATED.update(|mask| mask & !bit);
        }
    }
}
fn locally_passed(st: &State) -> u8 {
    st.keys.iter().fold(0, |mask, (&code, &swallowed)| {
        let key = KeyCode(code);
        if key.is_modifier() && !swallowed {
            mask | (1 << (key.usage() - 0xe0))
        } else {
            mask
        }
    }) & !INVALIDATED.get()
}
pub(super) fn at_boundary(requested: u8) -> u8 {
    if requested == 0 || !on_hook_thread() || KEYBOARD_DEPTH.get() != 0 {
        return 0;
    }
    let revision = KEYBOARD_REVISION.get();
    let candidates = with_state(|st| {
        if st.continuity.available() {
            st.passed_modifiers.candidates(requested, locally_passed(st))
        } else {
            0
        }
    })
    .unwrap_or(0);
    if candidates == 0 {
        return 0;
    }
    // A boundary read is deliberately synchronous. A stale desktop notification
    // must not turn GetAsyncKeyState's inaccessible-desktop zero into an UP.
    let sample = if observable_desktop() {
        let mut down = 0;
        for (index, vk) in VKS.into_iter().enumerate() {
            if candidates & (1 << index) != 0 {
                // SAFETY: read-only, only outside keyboard callbacks on the active desktop.
                if unsafe { GetAsyncKeyState(vk) } < 0 {
                    down |= 1 << index;
                }
            }
        }
        observable_desktop().then_some(down)
    } else {
        None
    };
    // Native queries can dispatch nested callbacks. Never apply a sample across
    // an intervening key event, even if the same modifier is held again.
    if revision != KEYBOARD_REVISION.get() {
        return 0;
    }
    retire(candidates, sample)
}
fn observable_desktop() -> bool {
    // GetAsyncKeyState also returns zero on UIPI/access failure. Restrict this
    // feature to the product's UIAccess user backend, never a plain-token test
    // process or the pre-login source. Check desktop hook access synchronously.
    if super::super::profile::current().capture_desktop != "Default"
        || !oink_winsvc::backend::uiaccess().unwrap_or(false)
        || !desktop_trace::capture_desktop_active()
    {
        return false;
    }
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_HOOKCONTROL, DESKTOP_READOBJECTS,
        OpenInputDesktop,
    };
    // SAFETY: read-only access check; close the owned handle on this thread.
    unsafe {
        let Ok(desktop) = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS.0 | DESKTOP_HOOKCONTROL.0),
        ) else {
            return false;
        };
        let _ = CloseDesktop(desktop);
    }
    true
}

fn retire(requested: u8, system_down: Option<u8>) -> u8 {
    let released = with_state(|st| {
        if KEYBOARD_DEPTH.get() != 0 || !st.continuity.available() {
            return 0;
        }
        let released = st.passed_modifiers.released(requested, locally_passed(st), system_down);
        for index in 0..8 {
            if released & (1 << index) != 0 {
                let code = KeyCode::keyboard(0xe0 + index);
                st.keys.remove(&code.0);
                external::retire_physical_modifier(VKS[index as usize] as usize);
                st.continuity.observe(&InputEvent::Key { code, pressed: false });
            }
        }
        st.passed_modifiers.invalidate(released);
        released
    })
    .unwrap_or(0);
    keyboard_trace::reconciliation(requested, system_down, released);
    released
}

#[cfg(all(test, debug_assertions))]
pub(super) fn with_sample_for_test(requested: u8, sample: Option<u8>) -> u8 {
    retire(requested, sample)
}
