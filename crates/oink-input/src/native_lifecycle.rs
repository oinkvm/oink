//! Numeric native lifecycle observations. Producers never format, log,
//! allocate or wait for the consumer. A full queue drops records and counts loss.
use crossbeam_queue::ArrayQueue;
use oink_foundation::{DeviceId, snapshot::Snapshot};
use serde::{Deserialize, Serialize};
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Instant;

static LIFECYCLE: AtomicBool = AtomicBool::new(false);
static LIFECYCLE_GENERATION: AtomicU64 = AtomicU64::new(0);
static BUFFER: OnceLock<Buffer> = OnceLock::new();
static WAKE: OnceLock<Snapshot<Option<crate::capture_loop::Wake>>> = OnceLock::new();
static PENDING: AtomicBool = AtomicBool::new(false);
type ClockSource = std::sync::Arc<dyn Fn() -> (u64, i64, i64) + Send + Sync>;
static CLOCK: OnceLock<Snapshot<Option<ClockSource>>> = OnceLock::new();
/// Use the flight recorder's capture clock; installed on the ordinary startup thread.
pub fn set_clock(clock: ClockSource) {
    CLOCK.get_or_init(|| Snapshot::new(None)).publish(Some(clock));
}

/// Ordinary startup registers a coalesced, allocation-free park notification.
/// Replace it when the backend resets; the closure must not retain the backend.
pub fn set_wake(wake: crate::capture_loop::Wake) {
    WAKE.get_or_init(|| Snapshot::new(None)).publish(Some(wake));
    PENDING.store(false, Ordering::Release);
    if BUFFER.get().is_some_and(|buffer| !buffer.queue.is_empty()) {
        notify();
    }
}
fn notify() {
    if !PENDING.swap(true, Ordering::AcqRel)
        && let Some(slot) = WAKE.get()
        && let Some(wake) = slot.load().as_ref()
    {
        wake();
    }
}
struct Buffer {
    start: Instant,
    queue: ArrayQueue<Record>,
    dropped: AtomicU64,
    sequence: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    HookInstall,
    HookRemove,
    HookReinstall,
    Mode,
    Hide,
    Show,
    Park,
    ClipApply,
    ClipChanged,
    Registration,
    ModeRoute,
    ModeRequest,
    CoverRequest,
    CoverApplied,
    CoverCursor,
    CoverReady,
    ParkCommitted,
    UncoverRequest,
    UncoverCommitted,
    ModeDecision,
    InputDesktop,
    Gesture,
    PointerTransition,
    ButtonTransition,
    /// Interval counts: Data.reason = ordinary_motion::Reason numeric code,
    /// Data.reentries = frame count.
    PointerAuthority,
    /// Ordinary park could not commit its one native input:
    /// Data.extra = cover generation; flags = native error (0 may be unknown),
    /// point = requested park; cursor = observed position/target when available.
    ParkInputSenderUnavailable,
    ParkInputSendRejected,
    ParkInputHookNotObserved,
    ParkInputTargetMismatch,
    ParkInputHideLost,
    ParkInputHookChainRejected,
    ParkInputReadbackMismatch,
    ParkInputCompletionUnavailable,
    /// message: 0=subscription started, 1=fault, 2=known-idle entry attestation,
    /// 3=local capture generation barrier (never a native fault).
    /// Fault reason: 1=frame shape,
    /// 2=state, 3=contact geometry/count, 4=duplicate ID, 5=queue overflow.
    /// No coordinates or contact IDs are recorded.
    ContactCapture,
    /// message: contact phase (0=Begin, 1=Move, 2=End, 3=Cancel),
    /// extra: submitted frame count; success: native receiver result.
    /// reason: 0=submission, 1=remote capture, 2=mode transition, 3=desktop unavailable.
    ContactInjection,
}

/// A failed park remains covered and is not retried within the same generation.
/// SendInput does not reliably identify UIPI, so SendRejected never guesses it.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ParkInputFailure {
    SenderUnavailable,
    SendRejected,
    HookNotObserved,
    TargetMismatch,
    HideLost,
    HookChainRejected,
    ReadbackMismatch,
    CompletionUnavailable,
}
#[cfg(windows)]
impl ParkInputFailure {
    pub(crate) fn kind(self) -> Kind {
        match self {
            Self::SenderUnavailable => Kind::ParkInputSenderUnavailable,
            Self::SendRejected => Kind::ParkInputSendRejected,
            Self::HookNotObserved => Kind::ParkInputHookNotObserved,
            Self::TargetMismatch => Kind::ParkInputTargetMismatch,
            Self::HideLost => Kind::ParkInputHideLost,
            Self::HookChainRejected => Kind::ParkInputHookChainRejected,
            Self::ReadbackMismatch => Kind::ParkInputReadbackMismatch,
            Self::CompletionUnavailable => Kind::ParkInputCompletionUnavailable,
        }
    }
}

/// Numeric lifecycle state, with no key payload or native process/window identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Data {
    pub message: u32,
    pub flags: u32,
    pub extra: u64,
    pub point: Option<[i32; 2]>,
    pub cursor: Option<[i32; 2]>,
    pub clip: Option<[i32; 4]>,
    pub remote: Option<bool>,
    pub hidden: Option<bool>,
    pub park: Option<[i32; 2]>,
    pub reentries: u64,
    pub cursor_showing: Option<bool>,
    /// Mode: 0=in hook, 1=marker, 2=fallback timer. Hook: 0=mouse, 1=keyboard.
    pub reason: u32,
    pub success: Option<bool>,
}
#[derive(Clone, Copy, Debug)]
pub struct Record {
    /// Transient stamp, not part of native wire/event serialization.
    pub stamp: Option<(u64, i64, i64)>,
    pub at_us: u64,
    pub sequence: u64,
    pub kind: Kind,
    pub data: Data,
    /// None means the thread has no router owner or it is currently borrowed.
    pub focus: Option<(DeviceId, u64)>,
    pub gesture: Option<crate::gesture_trace::Observation>,
    pub pointer: Option<crate::pointer_transition::Observation>,
    pub button: Option<crate::button_diagnostics::Transition>,
    pub contacts: Option<crate::contact_trace::Observation>,
    pub mt_contacts: Option<crate::mt_trace::Observation>,
}

/// Runtime only; allocation happens here on the API thread, never in a callback.
fn initialize() {
    BUFFER.get_or_init(Buffer::new);
}
/// Flight recorder lifecycle only: does not enable mouse/keyboard or periodic observation tracing.
pub fn set_lifecycle(enabled: bool) {
    if enabled {
        initialize();
    }
    if LIFECYCLE.swap(enabled, Ordering::Relaxed) != enabled {
        LIFECYCLE_GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}
pub fn lifecycle_generation() -> u64 {
    LIFECYCLE_GENERATION.load(Ordering::Relaxed)
}
/// Ordinary recorder boundary (pause/rekey); abandon statistical pairings only.
pub fn reset_gestures() {
    LIFECYCLE_GENERATION.fetch_add(1, Ordering::Relaxed);
}
pub fn capture_stamp() -> Option<(u64, i64, i64)> {
    CLOCK
        .get()
        .and_then(|slot| slot.load().as_ref().as_ref().map(|clock| clock()))
}
/// Only a boundary row enters the existing bounded queue. Changed stays numeric state.
pub fn emit_gesture(stamp: Option<(u64, i64, i64)>, gesture: crate::gesture_trace::Observation) {
    let Some(buffer) = BUFFER.get() else { return };
    let record = Record {
        stamp,
        at_us: buffer.start.elapsed().as_micros() as u64,
        sequence: buffer.sequence.fetch_add(1, Ordering::Relaxed),
        kind: Kind::Gesture,
        data: Data::default(),
        focus: crate::capture_loop::trace_focus(),
        gesture: Some(gesture),
        pointer: None,
        button: None,
        contacts: None,
        mt_contacts: None,
    };
    buffer.push(record);
    notify();
}
/// Raw button observations use the same bounded recorder queue, never a key payload.
pub(crate) fn emit_button(button: crate::button_diagnostics::Transition) {
    let Some(buffer) = BUFFER.get() else { return };
    let record = Record {
        stamp: capture_stamp(),
        at_us: timestamp(),
        sequence: buffer.sequence.fetch_add(1, Ordering::Relaxed),
        kind: Kind::ButtonTransition,
        data: Data::default(),
        focus: crate::capture_loop::trace_focus(),
        gesture: None,
        pointer: None,
        button: Some(button),
        contacts: None,
        mt_contacts: None,
    };
    buffer.push(record);
    notify();
}
/// A source transition, never an ordinary per-motion native trace row.
pub fn emit_pointer_transition(pointer: crate::pointer_transition::Observation) {
    if !lifecycle_enabled() {
        return;
    }
    let Some(buffer) = BUFFER.get() else { return };
    let record = Record {
        stamp: capture_stamp(),
        at_us: buffer.start.elapsed().as_micros() as u64,
        sequence: buffer.sequence.fetch_add(1, Ordering::Relaxed),
        kind: Kind::PointerTransition,
        data: Data::default(),
        focus: crate::capture_loop::trace_focus(),
        gesture: None,
        pointer: Some(pointer),
        button: None,
        contacts: None,
        mt_contacts: None,
    };
    buffer.push(record);
    notify();
}
/// Uses the recorder's monotonic clock (fallback only when no recorder is installed).
pub fn gesture_time(stamp: Option<(u64, i64, i64)>) -> u64 {
    stamp.map_or_else(
        || BUFFER.get().map_or(0, |b| b.start.elapsed().as_micros() as u64),
        |s| s.0,
    )
}
pub fn lifecycle_enabled() -> bool {
    LIFECYCLE.load(Ordering::Relaxed)
}
pub fn emit_lifecycle(kind: Kind, data: Data) {
    if lifecycle_enabled() {
        emit(kind, data);
    }
}
/// Append one numeric lifecycle observation to the bounded recorder queue.
pub fn emit(kind: Kind, data: Data) -> u64 {
    let Some(buffer) = BUFFER.get() else { return 0 };
    let sequence = buffer.sequence.fetch_add(1, Ordering::Relaxed);
    emit_record(kind, data, sequence, crate::capture_loop::trace_focus());
    sequence
}
/// A routing owner already borrowed by its event loop supplies its own focus.
pub fn emit_focused(kind: Kind, data: Data, focus: (DeviceId, u64)) {
    let Some(buffer) = BUFFER.get() else { return };
    let sequence = buffer.sequence.fetch_add(1, Ordering::Relaxed);
    emit_record(kind, data, sequence, Some(focus));
}
fn emit_record(kind: Kind, data: Data, sequence: u64, focus: Option<(DeviceId, u64)>) {
    let Some(buffer) = BUFFER.get() else { return };
    let stamp = capture_stamp();
    let record = Record {
        stamp,
        at_us: buffer.start.elapsed().as_micros() as u64,
        sequence,
        kind,
        data,
        focus,
        gesture: None,
        pointer: None,
        button: None,
        contacts: None,
        mt_contacts: None,
    };
    buffer.push(record);
    notify();
}
/// Per-native-call contact metadata shares the bounded recorder queue. It never
/// contains geometry and never changes native admission or contact lifetime.
#[cfg(any(windows, all(test, debug_assertions)))]
pub(crate) fn emit_contacts(contacts: crate::contact_trace::Observation) {
    if !lifecycle_enabled() {
        return;
    }
    let Some(buffer) = BUFFER.get() else { return };
    buffer.push(Record {
        stamp: capture_stamp(),
        at_us: timestamp(),
        sequence: buffer.sequence.fetch_add(1, Ordering::Relaxed),
        kind: Kind::ContactInjection,
        data: Data::default(),
        focus: crate::capture_loop::trace_focus(),
        gesture: None,
        pointer: None,
        button: None,
        contacts: Some(contacts),
        mt_contacts: None,
    });
    notify();
}
/// Bounded, sampled MT metadata; never geometry or raw device addresses.
#[cfg(target_os = "macos")]
pub(crate) fn emit_mt_contacts(mt_contacts: crate::mt_trace::Observation) {
    if !lifecycle_enabled() {
        return;
    }
    let Some(buffer) = BUFFER.get() else { return };
    buffer.push(Record {
        stamp: capture_stamp(),
        at_us: timestamp(),
        sequence: buffer.sequence.fetch_add(1, Ordering::Relaxed),
        kind: Kind::ContactCapture,
        data: Data::default(),
        focus: None,
        gesture: None,
        pointer: None,
        button: None,
        contacts: None,
        mt_contacts: Some(mt_contacts),
    });
    notify();
}
/// Ordinary thread only. Bound work even if producers continuously fill the queue.
pub fn drain(mut consume: impl FnMut(Record)) -> u64 {
    let Some(buffer) = BUFFER.get() else { return 0 };
    PENDING.store(false, Ordering::Release);
    buffer.drain(&mut consume)
}

pub(crate) fn timestamp() -> u64 {
    BUFFER.get().map_or(0, |b| b.start.elapsed().as_micros() as u64)
}

impl Buffer {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            queue: ArrayQueue::new(4096),
            dropped: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
        }
    }
    fn push(&self, record: Record) {
        if self.queue.push(record).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn drain(&self, mut consume: impl FnMut(Record)) -> u64 {
        for _ in 0..4096 {
            let Some(record) = self.queue.pop() else { break };
            consume(record);
        }
        self.dropped.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_lifecycle_queue_counts_loss_and_resumes_after_drain() {
        let buffer = Buffer::new();
        let record = Record {
            stamp: None,
            at_us: 0,
            sequence: 7,
            kind: Kind::Mode,
            data: Data {
                remote: Some(true),
                ..Data::default()
            },
            focus: None,
            gesture: None,
            pointer: None,
            button: None,
            contacts: None,
            mt_contacts: None,
        };
        for _ in 0..4100 {
            buffer.push(record);
        }
        let mut count = 0;
        assert_eq!(
            buffer.drain(|row| {
                count += 1;
                assert_eq!(row.kind, Kind::Mode);
                assert_eq!(row.data.remote, Some(true));
            }),
            4
        );
        assert_eq!(count, 4096);
        buffer.push(Record {
            kind: Kind::ParkInputSendRejected,
            ..record
        });
        let mut recovered = Vec::new();
        assert_eq!(buffer.drain(|row| recovered.push(row.kind)), 0);
        assert_eq!(recovered, [Kind::ParkInputSendRejected]);
    }
}
