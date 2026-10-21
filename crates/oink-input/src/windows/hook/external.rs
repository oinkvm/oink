//! External synthetic input is a native local stream by default. This state
//! lives outside State so a reentrant downstream hook cannot bypass ownership
//! or lose the native cursor commit while the router/capture state is borrowed.
use super::*;
use crate::windows_external_input::{Counters, ExternalInput, Kind, Origin, classify};
use std::sync::atomic::AtomicU32;
use windows::Win32::UI::WindowsAndMessaging::{
    LLKHF_LOWER_IL_INJECTED, LLMHF_LOWER_IL_INJECTED, WM_KEYUP, WM_SYSKEYUP,
};

thread_local! {
    static POLICY: RefCell<ExternalInput> = RefCell::new(ExternalInput::default());
    static REQUESTED: Cell<bool> = const { Cell::new(false) };
    static COUNTS: RefCell<(u64, Counters)> = RefCell::new((0, Counters::default()));
}

// One capture owner per Windows backend/process. Only observed physical key
// transitions write this fixed-size snapshot; no key identity is serialized.
static PHYSICAL_KEYS: [AtomicU32; 256] = [const { AtomicU32::new(0) }; 256];

pub(super) fn mouse_origin(info: &MSLLHOOKSTRUCT) -> Origin {
    classify(
        info.dwExtraInfo == INJECT_TAG,
        is_marker(info.dwExtraInfo),
        info.flags & LLMHF_INJECTED != 0,
        info.flags & LLMHF_LOWER_IL_INJECTED != 0,
    )
}

pub(super) fn key_origin(info: &KBDLLHOOKSTRUCT) -> Origin {
    classify(
        info.dwExtraInfo == INJECT_TAG,
        is_marker(info.dwExtraInfo),
        info.flags.0 & LLKHF_INJECTED.0 != 0,
        info.flags.0 & LLKHF_LOWER_IL_INJECTED.0 != 0,
    )
}

fn requested() -> bool {
    if let Some(value) = crate::capture_loop::forward_injected_input() {
        REQUESTED.set(value);
    }
    REQUESTED.get()
}

fn counts(f: impl FnOnce(&mut Counters)) {
    let generation = crate::native_lifecycle::lifecycle_generation();
    COUNTS.with_borrow_mut(|(old, counters)| {
        if *old != generation || !crate::native_lifecycle::lifecycle_enabled() {
            *old = generation;
            *counters = Counters::default();
        }
        if crate::native_lifecycle::lifecycle_enabled() {
            f(counters);
        }
    });
}

fn local(kind: Kind, lower: bool) -> bool {
    let requested = requested();
    let local = POLICY.with_borrow_mut(|policy| policy.local(kind, requested));
    counts(|c| c.observe(kind, lower, local));
    local
}

pub(super) fn mouse_local(msg: u32, info: &MSLLHOOKSTRUCT) -> bool {
    let kind = match msg {
        WM_MOUSEMOVE => Kind::Move,
        WM_LBUTTONDOWN | WM_LBUTTONUP => Kind::Button(0, msg == WM_LBUTTONDOWN),
        WM_RBUTTONDOWN | WM_RBUTTONUP => Kind::Button(1, msg == WM_RBUTTONDOWN),
        WM_MBUTTONDOWN | WM_MBUTTONUP => Kind::Button(2, msg == WM_MBUTTONDOWN),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            Kind::Button(((info.mouseData >> 16) + 2).min(255) as u8, msg == WM_XBUTTONDOWN)
        }
        WM_MOUSEWHEEL => Kind::VerticalWheel,
        WM_MOUSEHWHEEL => Kind::HorizontalWheel,
        _ => Kind::Other,
    };
    local(kind, info.flags & LLMHF_LOWER_IL_INJECTED != 0)
}

pub(super) fn key_local(msg: u32, info: &KBDLLHOOKSTRUCT) -> bool {
    let kind = match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => Kind::Key(info.vkCode as u8, true),
        WM_KEYUP | WM_SYSKEYUP => Kind::Key(info.vkCode as u8, false),
        _ => Kind::Other,
    };
    local(kind, info.flags.0 & LLKHF_LOWER_IL_INJECTED.0 != 0)
}

pub(super) fn pass_mouse(msg: u32, info: &MSLLHOOKSTRUCT, next: impl FnOnce() -> LRESULT) -> LRESULT {
    let _in_hook = InHook::enter();
    let acceptance = (msg == WM_MOUSEMOVE).then(|| {
        let before = cursor_pos_checked().ok();
        counts(|c| {
            c.base_registered += 1;
            c.position_read_failed += u64::from(before.is_none());
        });
        // The ledger borrow ends before CallNext can pump another native input.
        super::super::warp::accepted_external(before, info.pt)
    });
    let result = next();
    if result.0 != 0 {
        counts(|c| c.downstream_rejected += 1);
        if let Some(acceptance) = acceptance {
            let rolled_back = super::super::warp::rejected(acceptance);
            counts(|c| c.base_rollback += u64::from(rolled_back));
        }
    }
    result
}

pub(super) fn pass_key(next: impl FnOnce() -> LRESULT) -> LRESULT {
    let result = next();
    if result.0 != 0 {
        counts(|c| c.downstream_rejected += 1);
    }
    result
}

pub(super) fn observe_physical_key(msg: u32, info: &KBDLLHOOKSTRUCT) {
    let Some(slot) = PHYSICAL_KEYS.get(info.vkCode as usize) else {
        return;
    };
    match msg {
        WM_KEYUP | WM_SYSKEYUP => slot.store(0, Ordering::Relaxed),
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let mut scan = (info.scanCode & 0xff) as u16;
            if info.flags.0 & LLKHF_EXTENDED.0 != 0 {
                scan |= 0xe000;
            }
            if info.scanCode == 0 {
                // Match ordinary hook translation without reading merged key state.
                // SAFETY: a read-only virtual-key to scan-code lookup.
                scan = unsafe { MapVirtualKeyW(info.vkCode, MAPVK_VK_TO_VSC_EX) } as u16;
            }
            if let Some(code) = keymap::scancode_to_hid(scan) {
                slot.store(code.0, Ordering::Relaxed);
            }
        }
        _ => {}
    }
}

pub(crate) fn physical_keys() -> Vec<KeyCode> {
    PHYSICAL_KEYS
        .iter()
        .filter_map(|key| {
            let code = key.load(Ordering::Relaxed);
            (code != 0).then_some(KeyCode(code))
        })
        .collect()
}

pub(super) fn gap() {
    POLICY.with_borrow_mut(ExternalInput::gap);
    for key in &PHYSICAL_KEYS {
        key.store(0, Ordering::Relaxed);
    }
}

/// Existing one-second maintenance only; counters never drive capture policy.
/// Reset at recorder generation changes, including pause/rekey, before export.
pub(super) fn report(remote: bool, raw_clip_active: bool) {
    let mut snapshot = Counters::default();
    counts(|c| snapshot = std::mem::take(c));
    if snapshot == Counters::default() {
        return;
    }
    tracing::info!(target: "oink_windows_external_input",
        move_count = snapshot.move_count, button_down = snapshot.button_down,
        button_up = snapshot.button_up, wheel_vertical = snapshot.wheel_vertical,
        wheel_horizontal = snapshot.wheel_horizontal, key_down = snapshot.key_down,
        key_up = snapshot.key_up, other = snapshot.other, lower_il = snapshot.lower_il,
        local = snapshot.local, downstream_rejected = snapshot.downstream_rejected,
        base_registered = snapshot.base_registered, base_rollback = snapshot.base_rollback,
        position_read_failed = snapshot.position_read_failed, remote, raw_clip_active,
        "external synthetic input counts since previous report");
}
