//! Pair a locally accepted replacement UP with an exclusively local physical
//! press. This is not async-state reconciliation or external-input forwarding.
use super::*;
use std::collections::VecDeque;
use windows::Win32::UI::WindowsAndMessaging::{WM_KEYUP, WM_SYSKEYUP};

#[derive(Clone, Copy)]
pub(super) struct Release {
    code: KeyCode,
    generation: u64,
    info: KBDLLHOOKSTRUCT,
    time_us: u64,
    source_time: oink_foundation::input::CaptureTime,
}
#[derive(Default)]
struct Ledger {
    next: u64,
    passed: HashMap<KeyCode, u64>,
    pending: VecDeque<Release>,
}
thread_local! { static LEDGER: RefCell<Ledger> = RefCell::new(Ledger::default()); }

pub(super) fn first_down(code: KeyCode, physical_pass: bool) {
    LEDGER.with_borrow_mut(|ledger| {
        ledger.next = ledger.next.wrapping_add(1);
        if physical_pass {
            let generation = ledger.next;
            ledger.passed.insert(code, generation);
        } else {
            ledger.passed.remove(&code);
        }
    });
}
pub(super) fn physical_passed(code: KeyCode) -> bool {
    LEDGER.with_borrow(|ledger| ledger.passed.contains_key(&code))
}
pub(super) fn forget(code: KeyCode) {
    LEDGER.with_borrow_mut(|ledger| {
        ledger.passed.remove(&code);
    });
}
pub(super) fn clear() {
    LEDGER.with_borrow_mut(|ledger| {
        ledger.passed.clear();
        ledger.pending.clear();
    });
}
pub(super) fn capture(message: u32, info: &KBDLLHOOKSTRUCT) -> Option<Release> {
    if !matches!(message, WM_KEYUP | WM_SYSKEYUP) {
        return None;
    }
    let code = keymap::scancode_to_hid(keyboard_scancode(info)?)?;
    let generation = LEDGER.with_borrow(|ledger| ledger.passed.get(&code).copied())?;
    Some(Release {
        code,
        generation,
        info: *info,
        time_us: time_us(Instant::now()),
        source_time: crate::source_time::capture_time(),
    })
}
fn current(release: &Release) -> bool {
    LEDGER.with_borrow(|ledger| ledger.passed.get(&release.code) == Some(&release.generation))
}
fn attempt(release: Release) -> Option<bool> {
    if !current(&release) {
        return Some(false);
    }
    if !crate::capture_loop::callback_available() {
        return None;
    }
    let eligible = with_state(|st| st.continuity.available() && st.keys.get(&release.code.0) == Some(&false))?;
    if !eligible {
        return Some(false);
    }
    let event = CapturedEvent {
        event: InputEvent::Key {
            code: release.code,
            pressed: false,
        },
        physical: false,
        source: SourceKind::Unknown,
        time_us: release.time_us,
        cursor: None,
    };
    let _source_time = crate::source_time::CaptureScope::new(release.source_time);
    let retired = crate::diagnostics::with_capture(
        crate::diagnostics::CaptureMetadata {
            raw: crate::diagnostics::RawKeyEvent::Up,
            backend_generated: Some(true),
            repeat: Some(false),
            ..Default::default()
        },
        || crate::capture_loop::release_local_key(&event),
    );
    if retired {
        // The owner callback is pure bookkeeping and must not dispatch native
        // work. Thus no new native DOWN can overtake this press retirement.
        with_state(|st| {
            st.keys.remove(&release.code.0);
            st.continuity.observe(&event.event);
        });
        external::retire_physical_key(release.code);
        forget(release.code);
    }
    Some(retired)
}
pub(super) fn accepted(release: Option<Release>) {
    let Some(release) = release else {
        return;
    };
    if attempt(release).is_some() {
        return;
    }
    let queued = LEDGER.with_borrow_mut(|ledger| {
        if ledger
            .pending
            .iter()
            .any(|r| r.code == release.code && r.generation == release.generation)
        {
            return false;
        }
        ledger.pending.push_back(release);
        true
    });
    keyboard_trace::release(keyboard_trace::Stage::Deferred, &release.info, queued);
    if queued {
        #[cfg(all(test, debug_assertions))]
        if TEST_CURSOR.with(|cursor| cursor.get().is_some()) {
            return;
        }
        // SAFETY: post scalar arguments to this hook thread's existing
        // message queue, just like physical deferred releases; no input injection.
        unsafe {
            let _ = PostThreadMessageW(GetCurrentThreadId(), key_releases::MESSAGE, WPARAM(0), LPARAM(0));
        }
    }
}
pub(super) fn drain() {
    loop {
        let next = LEDGER.with_borrow(|ledger| ledger.pending.front().copied());
        let Some(release) = next else {
            break;
        };
        let Some(retired) = attempt(release) else {
            break;
        };
        LEDGER.with_borrow_mut(|ledger| {
            ledger.pending.pop_front();
        });
        keyboard_trace::release(keyboard_trace::Stage::Drained, &release.info, retired);
    }
}
