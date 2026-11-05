//! Modifier-only hook entrance/exit and boundary evidence. This never reads
//! asynchronous key state or changes capture, deferred releases or routing.
use super::*;
use crate::native_lifecycle::{self, Kind};
use crate::windows_keyboard_trace::{self as model, Counters};
pub(super) use crate::windows_keyboard_trace::{Outcome, Stage};

thread_local! {
    static COUNTERS: Cell<Counters> = Cell::new(Counters::default());
    static RECORDER: Cell<u64> = const { Cell::new(0) };
}
#[cfg(all(test, debug_assertions))]
thread_local! { pub(super) static TEST_RECORDS: RefCell<Option<Vec<native_lifecycle::Data>>> = const { RefCell::new(None) }; }
fn enabled() -> bool {
    #[cfg(all(test, debug_assertions))]
    if TEST_RECORDS.with_borrow(|r| r.is_some()) {
        return true;
    }
    native_lifecycle::lifecycle_enabled()
}
fn counters() -> Counters {
    let generation = native_lifecycle::lifecycle_generation();
    if RECORDER.replace(generation) != generation {
        COUNTERS.update(|mut c| {
            c.reset_recorder();
            c
        });
    }
    COUNTERS.get()
}
pub(super) fn held(st: &State) -> u8 {
    st.keys
        .keys()
        .map(|code| KeyCode(*code))
        .filter(|key| key.is_modifier())
        .map(|key| 1u8 << (key.usage() - 0xe0))
        .fold(0, |mask, bit| mask | bit)
}
fn emit(stage: Stage, serial: u64, flags: u32, known: Option<u8>, success: Option<bool>) {
    if !enabled() {
        return;
    }
    let mut data = counters().data(stage, serial, flags, known);
    data.success = success;
    data.chain_result = i64::try_from(native_lifecycle::lifecycle_generation()).ok();
    data.remote = with_state(|st| matches!(st.mode, CaptureMode::Remote { .. }));
    #[cfg(all(test, debug_assertions))]
    if TEST_RECORDS.with_borrow_mut(|records| {
        if let Some(records) = records {
            records.push(data);
            true
        } else {
            false
        }
    }) {
        return;
    }
    native_lifecycle::emit_lifecycle(Kind::KeyboardCapture, data);
}
pub(super) struct Capture {
    serial: u64,
    recorder: u64,
    flags: u32,
    outcome: Outcome,
}
impl Capture {
    pub(super) fn enter(msg: u32, info: &KBDLLHOOKSTRUCT) -> Option<Self> {
        let modifier = model::modifier(info.vkCode);
        if modifier == 0 || !enabled() {
            return None;
        }
        use windows::Win32::UI::WindowsAndMessaging::{WM_KEYUP, WM_SYSKEYUP};
        if !matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP) {
            return None;
        }
        let mut counts = counters();
        let serial = counts.enter();
        COUNTERS.set(counts);
        use crate::windows_external_input::Origin;
        let origin = match external::key_origin(info) {
            Origin::Physical => 0,
            Origin::External => 1,
            Origin::Own => 2,
            Origin::Marker => 3,
        };
        let flags = model::flags(
            modifier,
            matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN),
            matches!(msg, WM_SYSKEYDOWN | WM_SYSKEYUP),
            keyboard_scancode(info).and_then(keymap::scancode_to_hid).is_some(),
            origin,
            Outcome::Unfinished,
        );
        emit(Stage::Enter, serial, flags, with_state(|st| held(st)), None);
        Some(Self {
            serial,
            recorder: native_lifecycle::lifecycle_generation(),
            flags,
            outcome: Outcome::Unfinished,
        })
    }
    pub(super) fn outcome(&mut self, outcome: Outcome) {
        self.outcome = outcome;
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if self.recorder != native_lifecycle::lifecycle_generation() {
            return;
        }
        let mut counts = counters();
        counts.exit();
        COUNTERS.set(counts);
        emit(
            Stage::Exit,
            self.serial,
            self.flags | ((self.outcome as u32) << 16),
            with_state(|st| held(st)),
            None,
        );
    }
}
pub(super) fn outcome(capture: &mut Option<Capture>, outcome: Outcome) {
    if let Some(capture) = capture {
        capture.outcome(outcome);
    }
}
pub(super) fn release(stage: Stage, info: &KBDLLHOOKSTRUCT, success: bool) {
    let modifier = model::modifier(info.vkCode);
    if modifier != 0 {
        emit(
            stage,
            counters().entered,
            u32::from(modifier),
            with_state(|st| held(st)),
            Some(success),
        );
    }
}
pub(super) fn boundary(stage: Stage, detail: u32, known: Option<u8>, success: Option<bool>) {
    if stage == Stage::Installed {
        COUNTERS.update(|mut c| {
            c.install();
            c
        });
    }
    emit(stage, counters().entered, detail, known, success);
}

/// One numeric row per callback, including downstream CallNextHookEx time.
/// No key identity, formatting, allocation or unbounded histogram.
pub(super) fn duration(elapsed: Duration) {
    if !enabled() {
        return;
    }
    let mut data = counters().data(Stage::Duration, counters().entered, 0, None);
    data.steps = Some(elapsed.as_micros().min(u128::from(u32::MAX)) as u32);
    data.chain_result = i64::try_from(native_lifecycle::lifecycle_generation()).ok();
    native_lifecycle::emit_lifecycle(Kind::KeyboardCapture, data);
}
pub(super) fn reconciliation(requested: u8, system_down: Option<u8>, released: u8) {
    let mut data = counters().data(
        Stage::Reconcile,
        counters().entered,
        u32::from(requested) | (u32::from(released) << 16),
        system_down,
    );
    data.success = Some(system_down.is_some());
    data.chain_result = i64::try_from(native_lifecycle::lifecycle_generation()).ok();
    native_lifecycle::emit_lifecycle(Kind::KeyboardCapture, data);
}
