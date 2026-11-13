use super::{
    Reason, ReferenceSource, Summary, SystemStatus,
    reference::{Incoming, Kind, Reference},
};
use crate::{handoff, realtime::Signal};
use std::{
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Minimum spacing between engine rebuilds requested by the worker.
const REBUILD_COOLDOWN: Duration = Duration::from_millis(100);
/// A system reference that has not opened by then is not waited for: the Oink
/// render reference serves meanwhile (a later switch is a new epoch).
const SYSTEM_START_WAIT: Duration = Duration::from_secs(2);

pub(super) struct Update {
    pub epoch: u64,
    pub engine: Option<oink_aec3::Engine>,
    pub reference: Option<Reference>,
    /// Why this epoch replaced the previous one (diagnostics): the reason a
    /// worker requested a fresh engine, `OutputChanged` for a new or switched
    /// reference producer, `None` for a policy change or the first epoch.
    pub cause: Reason,
}
struct Side {
    incoming: [Option<Incoming>; 2],
    idle_reference: [Option<Reference>; 2],
    previous_mode: u8,
    previous_selected: u64,
    prepared_output: u64,
    epoch: u64,
    last_prepared: Option<Instant>,
    system_starting_since: Option<Instant>,
    system_detail: Option<String>,
    summaries: Option<handoff::Consumer<Summary>>,
}
/// One local shared-microphone processor and one physical forwarding exit.
/// Constructors, parameter changes and retirement run on ordinary owners.
/// Only fixed atomics and preallocated SPSC data cross the realtime boundary.
///
/// Two reference producers can be registered: the Oink render tap (always,
/// with the playback) and the full-system output mix (desktop, while AEC is
/// eligible). `update` selects exactly one; they are never added together,
/// because the system mix already contains Oink's output.
pub struct Control {
    pub(crate) wake: Signal,
    side: Mutex<Side>,
    mode: AtomicU8,
    capture_alive: AtomicBool,
    /// Registered producer generation per `Kind`, 0 when none.
    outputs: [AtomicU64; 2],
    /// The generation `update` selected; the only producer that pushes.
    selected: AtomicU64,
    next_output: AtomicU64,
    reset: AtomicU8,
    pending: AtomicPtr<Update>,
    retired: AtomicPtr<Update>,
    pub(super) reference_dropped: AtomicU64,
    engines: AtomicU64,
    /// A render callback reported a (new) format since the last `update`:
    /// a reference can be prepared now.
    pub(super) format_changed: AtomicBool,
    system_status: AtomicU8,
    source: AtomicU8,
    /// Thread CPU time of the system reference capture worker (µs, swapped
    /// out by each published summary) and its fixed allocation in bytes.
    pub(super) system_cpu_us: AtomicU64,
    pub(super) system_bytes: AtomicU64,
}
impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalEchoControl")
            .field("mode", &self.mode.load(Ordering::Relaxed))
            .field("source", &self.reference_source())
            .field("system", &self.system_status())
            .finish_non_exhaustive()
    }
}
impl Control {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            wake: Signal::new()?,
            side: Mutex::new(Side {
                incoming: [None, None],
                idle_reference: [None, None],
                previous_mode: 1,
                previous_selected: 0,
                prepared_output: 0,
                epoch: 0,
                last_prepared: None,
                system_starting_since: None,
                system_detail: None,
                summaries: None,
            }),
            mode: AtomicU8::new(1),
            capture_alive: AtomicBool::new(false),
            outputs: [AtomicU64::new(0), AtomicU64::new(0)],
            selected: AtomicU64::new(0),
            next_output: AtomicU64::new(0),
            reset: AtomicU8::new(0),
            pending: AtomicPtr::new(ptr::null_mut()),
            retired: AtomicPtr::new(ptr::null_mut()),
            reference_dropped: AtomicU64::new(0),
            engines: AtomicU64::new(0),
            format_changed: AtomicBool::new(false),
            system_status: AtomicU8::new(SystemStatus::NotRequested as u8),
            source: AtomicU8::new(ReferenceSource::None as u8),
            system_cpu_us: AtomicU64::new(0),
            system_bytes: AtomicU64::new(0),
        })
    }
    fn side(&self) -> std::sync::MutexGuard<'_, Side> {
        self.side.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(super) fn register(&self, mut incoming: Incoming) -> u64 {
        let generation = self.next_output.fetch_add(1, Ordering::Relaxed) + 1;
        incoming.generation = generation;
        let kind = incoming.kind as usize;
        let mut side = self.side();
        side.incoming[kind] = Some(incoming);
        side.idle_reference[kind] = None;
        self.outputs[kind].store(generation, Ordering::Release);
        self.wake.notify();
        generation
    }
    pub(super) fn output_stopped(&self, generation: u64) {
        for output in &self.outputs {
            let _ = output.compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Relaxed);
        }
        // Revoke at once: the processor bypasses until `update` selects again.
        let _ = self
            .selected
            .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Relaxed);
        self.wake.notify();
    }
    pub(super) fn wanted(&self, generation: u64) -> bool {
        self.mode.load(Ordering::Acquire) == 2 && self.selected.load(Ordering::Acquire) == generation
    }

    /// Reported by the system reference owner (ordinary threads only).
    /// `detail` is the platform's own failure text, kept for the diagnostic log.
    pub(super) fn set_system_status(&self, status: SystemStatus, detail: Option<String>, now: Instant) {
        let mut side = self.side();
        side.system_starting_since = (status == SystemStatus::Starting).then_some(now);
        if detail.is_some() || status == SystemStatus::Running || status == SystemStatus::NotRequested {
            side.system_detail = detail;
        }
        self.system_status.store(status as u8, Ordering::Release);
        drop(side);
        self.wake.notify();
    }
    /// Why the full-system reference is (not) in use, for diagnostics.
    pub fn system_status(&self) -> SystemStatus {
        SystemStatus::from_u8(self.system_status.load(Ordering::Acquire))
    }
    /// The platform's text for the last system reference failure, if any.
    pub fn system_detail(&self) -> Option<String> {
        self.side().system_detail.clone()
    }
    /// Which reference the processor currently cancels against.
    pub fn reference_source(&self) -> ReferenceSource {
        ReferenceSource::from_u8(self.source.load(Ordering::Acquire))
    }

    /// Choose the reference: the full-system mix when it runs, otherwise the
    /// Oink render reference. While the system capture is still opening (up
    /// to `SYSTEM_START_WAIT`) nothing is selected, so a normal start builds
    /// a single engine on the system reference.
    fn select(&self, side: &Side, now: Instant) -> (u64, ReferenceSource) {
        let system = self.outputs[Kind::System as usize].load(Ordering::Acquire);
        let oink = self.outputs[Kind::Oink as usize].load(Ordering::Acquire);
        match self.system_status() {
            SystemStatus::Running if system != 0 => (system, ReferenceSource::System),
            SystemStatus::Starting
                if side
                    .system_starting_since
                    .is_some_and(|t| now.saturating_duration_since(t) < SYSTEM_START_WAIT) =>
            {
                (0, ReferenceSource::None)
            }
            _ if oink != 0 => (oink, ReferenceSource::Oink),
            _ => (0, ReferenceSource::None),
        }
    }

    /// No group preference or protocol value is written. Called by the existing
    /// audio owner after evaluating both independent claims and mic demand.
    ///
    /// This is the only place an AEC3 engine is created, and it may also free
    /// a retired one: call it on an ordinary owner thread, never from a native
    /// audio callback or the capture worker. Owners call it on every
    /// maintenance pass while the microphone runs so a reset requested by the
    /// worker (`reset_pending`) is served; see `service_at` for the deadline.
    pub fn update(&self, enabled: bool, local_mic: bool, local_exit: bool, now: Instant) {
        self.reap();
        self.format_changed.store(false, Ordering::Release);
        let mut side = self.side();
        let (selected, source) = self.select(&side, now);
        let eligible = super::eligible(
            enabled,
            local_mic,
            local_exit,
            self.capture_alive.load(Ordering::Acquire),
        ) && selected != 0;
        let mode = if !enabled {
            0
        } else if eligible {
            2
        } else {
            1
        };
        self.mode.store(mode, Ordering::Release);
        self.selected.store(selected, Ordering::Release);
        self.source.store(
            if eligible { source } else { ReferenceSource::None } as u8,
            Ordering::Release,
        );
        let requested_reason = self.reset.load(Ordering::Acquire);
        let requested = requested_reason != 0;
        let cooldown = side
            .last_prepared
            .is_none_or(|t| now.saturating_duration_since(t) >= REBUILD_COOLDOWN);
        let kind = source.kind();
        let new_reference = eligible
            && kind.is_some_and(|k| {
                side.incoming[k as usize]
                    .as_ref()
                    .is_some_and(|i| i.generation == selected && i.rate.load(Ordering::Acquire) != 0)
            });
        let idle_reference = eligible
            && kind.is_some_and(|k| {
                side.idle_reference[k as usize]
                    .as_ref()
                    .is_some_and(|r| r.generation == selected)
            });
        let switched = eligible && side.previous_selected != selected;
        let changed = side.previous_mode != mode || new_reference || idle_reference || switched;
        if !changed && !(eligible && requested && cooldown) {
            return;
        }
        let reference = if new_reference {
            let incoming = side.incoming[kind.unwrap() as usize].take().unwrap();
            let rate = incoming.rate.load(Ordering::Acquire);
            side.prepared_output = selected;
            Some(Reference::new(incoming, rate))
        } else if idle_reference {
            side.prepared_output = selected;
            side.idle_reference[kind.unwrap() as usize].take()
        } else {
            None
        };
        // A new native output may not have reported its format yet. Keep raw
        // mic available and retry when the first callback makes it known.
        let engine = if eligible && (reference.is_some() || side.prepared_output == selected) {
            let engine = oink_aec3::Engine::try_new().ok();
            self.engines.fetch_add(u64::from(engine.is_some()), Ordering::Relaxed);
            engine
        } else {
            None
        };
        side.previous_mode = mode;
        if eligible {
            side.previous_selected = selected;
        }
        side.epoch = side.epoch.wrapping_add(1);
        side.last_prepared = Some(now);
        let cause = if eligible && requested && cooldown {
            Reason::from_u8(requested_reason)
        } else if new_reference || idle_reference || switched {
            Reason::OutputChanged
        } else {
            Reason::None
        };
        let mut update = Box::new(Update {
            epoch: side.epoch,
            engine,
            reference,
            cause,
        });
        let previous = self.pending.swap(ptr::null_mut(), Ordering::AcqRel);
        // Coalesce unconsumed updates without losing the unique reference reader.
        if !previous.is_null() {
            // SAFETY: the atomic swap above transferred exclusive ownership.
            let mut previous = unsafe { Box::from_raw(previous) };
            if let Some(old) = previous.reference.take() {
                if update.reference.is_none() && old.generation == selected {
                    update.reference = Some(old);
                } else {
                    Self::park(&self.outputs, &mut side, old);
                }
            }
        }
        drop(side);
        self.pending.store(Box::into_raw(update), Ordering::Release);
        self.reset.store(0, Ordering::Release);
        self.wake.notify();
    }
    /// Keep a still-registered reader for a later switch back; a reader whose
    /// producer is gone is dropped here, on the ordinary owner.
    fn park(outputs: &[AtomicU64; 2], side: &mut Side, reference: Reference) {
        let kind = reference.kind as usize;
        if outputs[kind].load(Ordering::Acquire) == reference.generation {
            side.idle_reference[kind] = Some(reference);
        }
    }
    fn reap(&self) {
        let retired = self.retired.swap(ptr::null_mut(), Ordering::AcqRel);
        if !retired.is_null() {
            // SAFETY: retirement transferred the worker's last ownership.
            let mut retired = unsafe { Box::from_raw(retired) };
            if let Some(reference) = retired.reference.take() {
                let mut side = self.side();
                Self::park(&self.outputs, &mut side, reference);
            }
        }
    }
    pub(super) fn mode(&self) -> u8 {
        self.mode.load(Ordering::Acquire)
    }
    pub(super) fn output(&self) -> u64 {
        self.selected.load(Ordering::Acquire)
    }
    pub(super) fn request_reset(&self, reason: Reason) {
        self.reset.store(reason as u8, Ordering::Release);
    }
    /// The worker asked for a fresh engine and the owner has not served it.
    /// Realtime safe: a single atomic load.
    pub fn reset_pending(&self) -> bool {
        self.reset.load(Ordering::Acquire) != 0
    }
    /// An owner pass would do work now: a requested reset, or a first
    /// reference/engine for an output whose format just became known.
    /// Realtime safe; owners without a fixed cadence wake on it.
    pub fn needs_service(&self) -> bool {
        self.reset_pending() || (self.mode.load(Ordering::Acquire) == 2 && self.format_changed.load(Ordering::Acquire))
    }
    /// When an owner must call `update` again to serve a pending reset: the
    /// end of the rebuild cooldown. None when nothing is pending or the
    /// processor is not eligible (no engine would be built).
    pub fn service_at(&self) -> Option<Instant> {
        if !self.reset_pending() || self.mode.load(Ordering::Acquire) != 2 {
            return None;
        }
        let side = self.side();
        Some(side.last_prepared.map_or_else(Instant::now, |t| t + REBUILD_COOLDOWN))
    }
    /// Engines created so far (diagnostics; a steady configuration builds one).
    pub fn engines_built(&self) -> u64 {
        self.engines.load(Ordering::Relaxed)
    }
    pub(super) fn set_summaries(&self, consumer: handoff::Consumer<Summary>) {
        self.side().summaries = Some(consumer);
    }
    /// The capture side's end of the per-window summary handoff (the owner
    /// reads it with `take_summary`). Creating one replaces an earlier one.
    /// Call on an ordinary thread, before capture runs.
    pub fn summary_publisher(&self) -> SummaryPublisher {
        let (producer, consumer) = handoff::channel(4, Summary::default());
        self.set_summaries(consumer);
        SummaryPublisher(producer)
    }
    /// One per-second processor summary, published by the capture side
    /// without formatting; the owner logs it. Ordinary threads only.
    pub fn take_summary(&self) -> Option<Summary> {
        self.side().summaries.as_mut()?.pop()
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        // All native/worker owners hold an Arc. Last-owner destruction therefore
        // occurs only after those owners have quiesced, on their ordinary owner.
        self.reap();
        let pending = self.pending.swap(ptr::null_mut(), Ordering::AcqRel);
        if !pending.is_null() {
            // SAFETY: last Arc owner, so no worker can acquire this pending box.
            unsafe {
                drop(Box::from_raw(pending));
            }
        }
    }
}
/// Hands fixed-value processor summaries from the capture worker to the owner.
/// Realtime safe: a full queue (owner not draining) loses a summary, never audio.
#[derive(Debug)]
pub struct SummaryPublisher(handoff::Producer<Summary>);
impl SummaryPublisher {
    pub fn publish(&mut self, summary: Summary) {
        let _ = self.0.push(summary);
    }
}
/// Lives in the single DSP executor. It is returned through JoinHandle before
/// destruction so the last reference, engine and queues never drop in realtime.
pub(crate) struct WorkerControl {
    pub control: Arc<Control>,
    pub(super) active: Option<Box<Update>>,
}
impl WorkerControl {
    pub fn new(control: Arc<Control>) -> Result<Self, String> {
        if control.capture_alive.swap(true, Ordering::AcqRel) {
            return Err("shared microphone DSP already exists".into());
        }
        Ok(Self { control, active: None })
    }
    pub fn apply(&mut self) -> bool {
        if !self.control.retired.load(Ordering::Acquire).is_null() {
            return false;
        }
        let pending = self.control.pending.swap(ptr::null_mut(), Ordering::AcqRel);
        if pending.is_null() {
            return false;
        }
        // SAFETY: unique pointer transferred by atomic swap; creating a Box from
        // an existing allocation does not allocate. Old ownership goes to reap.
        let mut update = unsafe { Box::from_raw(pending) };
        if let Some(mut old) = self.active.take() {
            if update.reference.is_none()
                && old
                    .reference
                    .as_ref()
                    .is_some_and(|r| r.generation == self.control.output())
            {
                update.reference = old.reference.take();
            }
            // The owner's reap parks a still-registered reader of the other
            // source; nothing is freed here.
            self.control.retired.store(Box::into_raw(old), Ordering::Release);
        }
        if let Some(reference) = &mut update.reference {
            reference.reset_timeline();
        }
        self.active = Some(update);
        true
    }
}
impl Drop for WorkerControl {
    fn drop(&mut self) {
        self.control.capture_alive.store(false, Ordering::Release);
        let _ = self
            .control
            .mode
            .compare_exchange(2, 1, Ordering::AcqRel, Ordering::Relaxed);
        self.control.reap();
        if let Some(active) = &mut self.active {
            let mut side = self.control.side();
            if let Some(reference) = active.reference.take() {
                Control::park(&self.control.outputs, &mut side, reference);
            }
            side.previous_mode = 1;
            side.previous_selected = 0;
        }
    }
}
