//! The injection thread. A thread of
//! its own because native injection can block (on Windows protected installs it is a
//! synchronous round trip to the input service helper). It does no drag-and-drop, no
//! display wake-up, no media control.
//!
//! The queue from the input thread is a hand-over between two realtime threads inside
//! the process; it is not a stream.
use oink_foundation::handoff_timing::{self, Stage, Token};
use oink_foundation::{
    DeviceId, Point,
    input::{CaptureMode, InjectEvent},
};

/// Input thread → injection thread. Events keep queue order; an older queued mode
/// is skipped if a newer mode was already applied inside a capture callback.
#[derive(Clone, Debug)]
pub enum InjectItem {
    /// Update ACK follows all preceding releases and native mode operations.
    UpdateFence(std::sync::Arc<std::sync::atomic::AtomicU8>),
    OwnerBegin(std::sync::Arc<std::sync::atomic::AtomicU64>),
    OwnerFence {
        state: std::sync::Arc<std::sync::atomic::AtomicU8>,
        failures: std::sync::Arc<std::sync::atomic::AtomicU64>,
        expect_released: bool,
    },
    /// Inject one event; `from` is the peer whose key state it belongs to.
    Inject {
        event: InjectEvent,
        from: Option<DeviceId>,
    },
    /// Non-callback mode switches keep their order relative to injected events.
    SetMode(CaptureMode),
    SetRelativeCapture(bool),
    RestoreLocal {
        at: Point,
    },
    /// Release everything `peer` holds on this device, then tell the input thread.
    ReleaseAll {
        peer: DeviceId,
    },
    /// An ordered handoff to the ordinary DnD worker. Begin permits subsequent
    /// pointer motion while preparing the native session; other events wait for
    /// completion. No native DnD call runs on this thread.
    Dnd {
        command: oink_routing::DndCmd,
        main: super::MainSender,
    },
    Stop,
}

/// A routed input event transferred to an ordinary worker still belongs to the
/// same input session and retains its queue age until completion or discard.
#[derive(Debug)]
pub struct OrdinaryInput {
    pub safety: oink_input::session_gate::InputSafety,
    at: std::time::Instant,
    _pending: oink_foundation::pending_age::Pending,
}
impl OrdinaryInput {
    pub fn expired(&self) -> bool {
        self.at.elapsed() > std::time::Duration::from_secs(5)
    }
}
struct SourceRequest {
    reply: std::sync::mpsc::SyncSender<DndCompletion>,
    work: OrdinaryInput,
}

/// Completion of one ordinary drag command. Dropping the command (including a
/// closed ordinary queue or early return) also retires the dependency.
#[derive(Debug)]
pub struct DndCompletion {
    pub(crate) work: Option<OrdinaryInput>,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::Thread,
    gate: std::sync::Arc<oink_foundation::wake::WakeGate>,
    source_motion: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}
impl DndCompletion {
    fn finish(&mut self) -> u64 {
        let dropped = self.source_motion.take().map_or(0, |state| {
            state.fetch_and(!SOURCE_ACTIVE, std::sync::atomic::Ordering::AcqRel) & !SOURCE_ACTIVE
        });
        self.ready.store(true, std::sync::atomic::Ordering::Release);
        if self.gate.notify() {
            self.thread.unpark();
        }
        dropped
    }
}
impl Drop for DndCompletion {
    fn drop(&mut self) {
        self.finish();
    }
}
const SOURCE_ACTIVE: u64 = 1 << 63;

/// Writer end of the injection queue, held by the input thread.
type ReleasedCallback = std::sync::Arc<std::sync::Mutex<Box<dyn Fn(DeviceId) + Send>>>;

pub struct InjectHandle {
    pub(crate) trace: Option<std::sync::Arc<crate::diagnostics::input_trace::InputTrace>>,
    queue: std::sync::Arc<crossbeam_queue::ArrayQueue<Queued>>,
    scheduler: std::sync::Arc<oink_foundation::snapshot::Snapshot<oink_network::link::SchedulerStatus>>,
    thread: std::sync::Arc<std::sync::Mutex<std::thread::Thread>>,
    gate: std::sync::Arc<oink_foundation::wake::WakeGate>,
    join: Option<std::thread::JoinHandle<()>>,
    spawn: Box<dyn Fn() -> std::io::Result<std::thread::JoinHandle<()>> + Send + Sync>,
    suspended: std::sync::Arc<std::sync::atomic::AtomicBool>,
    released_callback: ReleasedCallback,
    posting: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    backlog_trigger: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ordinary_age: std::sync::Arc<oink_foundation::pending_age::PendingAge>,
    backend: std::sync::Arc<dyn oink_input::InputBackend>,
    released: std::sync::Arc<crossbeam_queue::ArrayQueue<DeviceId>>,
    wake: std::sync::Arc<std::sync::OnceLock<oink_network::link::Wake>>,
    stopping: std::sync::atomic::AtomicBool,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    next_mode: std::sync::atomic::AtomicU64,
    failures: std::sync::Arc<std::sync::atomic::AtomicU64>,
    faults: std::sync::Arc<super::input::faults::NativeFaults>,
    mode_applied: std::sync::Arc<std::sync::Mutex<u64>>,
    source_starts: std::sync::Arc<crossbeam_queue::ArrayQueue<SourceRequest>>,
    source_motion: std::sync::Arc<std::sync::atomic::AtomicU64>,
    timing: std::sync::Arc<oink_input::timing::InjectionTiming>,
    handoff_epoch: std::sync::atomic::AtomicU64,
    first_motion_epoch: std::sync::atomic::AtomicU64,
}
impl std::fmt::Debug for InjectHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InjectHandle")
            .field("queued", &self.queue.len())
            .finish_non_exhaustive()
    }
}

// The input reader reserves room before taking another I0 message. Cleanup and mode
// barriers use the final quarter so a stopped consumer cannot prevent detachment.
const CAPACITY: usize = 8192;
fn session_sensitive(item: &InjectItem) -> bool {
    matches!(
        item,
        InjectItem::Inject { .. }
            | InjectItem::SetMode(_)
            | InjectItem::RestoreLocal { .. }
            | InjectItem::SetRelativeCapture(true)
            | InjectItem::Dnd {
                command: oink_routing::DndCmd::Begin { .. }
                    | oink_routing::DndCmd::HoldSource { .. }
                    | oink_routing::DndCmd::ResumeSource { .. }
                    | oink_routing::DndCmd::PrepareDrop { .. }
                    | oink_routing::DndCmd::Probe { .. },
                ..
            }
    )
}
struct Queued {
    safety: oink_input::session_gate::InputSafety,
    crossing: Option<Token>,
    first_motion: bool,
    item: InjectItem,
    at: std::time::Instant,
    mode_sequence: u64,
    _pending: oink_input::timing::Pending,
    source_time: Option<oink_input::source_time::CaptureTime>,
    report: Option<crate::telemetry::Stamp>,
}
struct Posting<'a>(&'a std::sync::atomic::AtomicUsize);
impl Drop for Posting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Release);
    }
}
impl InjectHandle {
    pub(crate) fn detached(&self) -> bool {
        self.stopped() && self.join.as_ref().is_none_or(|join| join.is_finished())
    }
    pub(crate) fn take_backlog_trigger(&self) -> bool {
        self.backlog_trigger.swap(false, std::sync::atomic::Ordering::AcqRel)
    }
    pub(crate) fn oldest_pending(&self) -> Option<std::time::Instant> {
        self.timing
            .oldest_pending()
            .into_iter()
            .chain(self.ordinary_age.oldest_in_epoch(self.backend.input_safety().epoch))
            .min()
    }
    pub(crate) fn ordinary_input(&self, safety: oink_input::session_gate::InputSafety) -> Option<OrdinaryInput> {
        let at = std::time::Instant::now();
        self.ordinary_age.discard_before(safety.epoch);
        Some(OrdinaryInput {
            safety,
            at,
            _pending: self.ordinary_age.track_epoch(at, safety.epoch)?,
        })
    }
    pub(crate) fn discard_pending(&self) {
        while let Some(queued) = self.queue.pop() {
            if let InjectItem::OwnerFence { state, .. } | InjectItem::UpdateFence(state) = queued.item {
                state.store(2, std::sync::atomic::Ordering::Release);
            }
        }
        while self.source_starts.pop().is_some() {}
        if let Some(wake) = self.wake.get() {
            wake();
        }
    }
    /// Called by the routing owner. The running worker performs its finite
    /// cleanup and exits; this call never joins a native operation in progress.
    pub(crate) fn suspend(&self) {
        self.suspended.store(true, std::sync::atomic::Ordering::Release);
        if self.gate.notify() {
            self.thread.lock().unwrap_or_else(|e| e.into_inner()).unpark();
        }
    }
    pub(crate) fn suspended(&self) -> bool {
        self.suspended.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn resume(&mut self) -> std::io::Result<bool> {
        if !self.suspended() {
            return Ok(true);
        }
        if !self.stopped() || self.join.as_ref().is_some_and(|join| !join.is_finished()) {
            return Ok(false);
        }
        if let Some(old) = self.join.take() {
            let _ = old.join();
        }
        self.suspended.store(false, std::sync::atomic::Ordering::Release);
        self.stopped.store(false, std::sync::atomic::Ordering::Release);
        match (self.spawn)() {
            Ok(join) => {
                *self.thread.lock().unwrap_or_else(|e| e.into_inner()) = join.thread().clone();
                self.join = Some(join);
                Ok(true)
            }
            Err(error) => {
                self.suspended.store(true, std::sync::atomic::Ordering::Release);
                self.stopped.store(true, std::sync::atomic::Ordering::Release);
                Err(error)
            }
        }
    }
    /// A dormant owner has no held input or native work. Control acknowledgments
    /// can complete without resurrecting an injection thread while paused.
    fn complete_dormant(&self, item: InjectItem) {
        use std::sync::atomic::Ordering;
        match item {
            InjectItem::OwnerBegin(baseline) => {
                baseline.store(self.failures.load(Ordering::Acquire), Ordering::Release)
            }
            InjectItem::OwnerFence { state, .. } | InjectItem::UpdateFence(state) => state.store(1, Ordering::Release),
            InjectItem::ReleaseAll { peer } => {
                assert!(self.released.push(peer).is_ok(), "one release per detached peer");
                (self.released_callback.lock().unwrap_or_else(|e| e.into_inner()))(peer);
            }
            InjectItem::Dnd { command, main }
                if !session_sensitive(&InjectItem::Dnd {
                    command: command.clone(),
                    main: main.clone(),
                }) =>
            {
                let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let completion = DndCompletion {
                    work: None,
                    ready,
                    thread: std::thread::current(),
                    gate: self.gate.clone(),
                    source_motion: None,
                };
                let _ = main.send(super::MainCommand::Dnd {
                    command,
                    completion,
                    report: None,
                    safety: self.backend.input_safety(),
                });
            }
            _ => {}
        }
        if let Some(wake) = self.wake.get() {
            wake();
        }
    }
    pub(crate) fn crossing(&self, epoch: u64, incoming: bool) {
        self.handoff_epoch.store(epoch, std::sync::atomic::Ordering::Relaxed);
        if incoming {
            self.first_motion_epoch
                .store(epoch, std::sync::atomic::Ordering::Relaxed);
        }
    }
    pub(crate) fn resource_faults(&self) -> super::input::ResourceFaults {
        self.faults.snapshot()
    }
    pub(crate) fn failure_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.failures.clone()
    }

    pub(crate) fn source_start(&self) -> oink_clipboard::dnd::SourceStart {
        let requests = self.source_starts.clone();
        let gate = self.gate.clone();
        let thread = self.thread.clone();
        let stopped = self.stopped.clone();
        let suspended = self.suspended.clone();
        let backend = self.backend.clone();
        let age = self.ordinary_age.clone();
        let producers = self.posting.clone();
        let backlog = self.backlog_trigger.clone();
        let input_wake = self.wake.clone();
        std::sync::Arc::new(move |work| {
            producers.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let posting = Posting(&producers);
            if stopped.load(std::sync::atomic::Ordering::Acquire)
                || suspended.load(std::sync::atomic::Ordering::Acquire)
            {
                return Err("injection owner stopped".into());
            }
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            let at = std::time::Instant::now();
            let safety = backend.input_safety();
            age.discard_before(safety.epoch);
            let pending = age.track_epoch(at, safety.epoch).ok_or("ordinary input queue full")?;
            requests
                .push(SourceRequest {
                    reply: send,
                    work: OrdinaryInput {
                        safety,
                        at,
                        _pending: pending,
                    },
                })
                .map_err(|_| "source start already pending".to_string())?;
            drop(posting);
            if gate.notify() {
                thread.lock().unwrap_or_else(|e| e.into_inner()).unpark();
            }
            let mut completion = receive.recv().map_err(|_| "injection owner stopped".to_string())?;
            if !backend.input_safety().accepts(safety) {
                return Err("source start belongs to an obsolete input session".into());
            }
            if completion.work.as_ref().is_some_and(OrdinaryInput::expired) {
                backlog.store(true, std::sync::atomic::Ordering::Release);
                backend.set_input_paused(true);
                if let Some(wake) = input_wake.get() {
                    wake();
                }
                return Err("source start exceeded input backlog deadline".into());
            }
            work();
            let dropped = completion.finish();
            Ok(dropped)
        })
    }
    /// Never blocks. The input thread checks `available` before reading I0.
    pub fn push(&self, item: InjectItem) {
        self.push_timed(item, None);
    }
    pub fn push_timed(&self, item: InjectItem, source_time: Option<oink_input::source_time::CaptureTime>) {
        self.push_reported(item, source_time, None);
    }
    pub fn push_reported(
        &self,
        item: InjectItem,
        source_time: Option<oink_input::source_time::CaptureTime>,
        report: Option<crate::telemetry::Stamp>,
    ) {
        self.push_reported_in_epoch(self.backend.input_safety(), item, source_time, report);
    }
    pub(crate) fn push_in_epoch(&self, safety: oink_input::session_gate::InputSafety, item: InjectItem) {
        self.push_reported_in_epoch(safety, item, None, None);
    }
    pub(crate) fn push_reported_in_epoch(
        &self,
        safety: oink_input::session_gate::InputSafety,
        item: InjectItem,
        source_time: Option<oink_input::source_time::CaptureTime>,
        report: Option<crate::telemetry::Stamp>,
    ) {
        if let (Some(trace), InjectItem::Dnd { command, .. }) = (&self.trace, &item) {
            trace.dnd_command(command, "CommandQueued", None, None);
        }
        let sequence = if matches!(item, InjectItem::SetMode(_) | InjectItem::RestoreLocal { .. }) {
            self.next_mode.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
        } else {
            0
        };
        self.record_mode_request(&item, sequence);
        trace_mode_route(&item, sequence, 2, None);
        self.enqueue_reported(safety, item, sequence, source_time, report);
    }
    fn record_mode_request(&self, item: &InjectItem, sequence: u64) {
        if let Some(trace) = &self.trace {
            match item {
                InjectItem::SetMode(mode) => trace.mode_requested(Some(*mode), None, sequence),
                InjectItem::RestoreLocal { at } => trace.mode_requested(Some(CaptureMode::Local), Some(*at), sequence),
                _ => {}
            }
        }
    }

    pub(crate) fn discard_source_motion(&self) -> bool {
        self.source_motion
            .try_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |state| (state & SOURCE_ACTIVE != 0).then_some(state + 1),
            )
            .is_ok()
    }
    fn enqueue_reported(
        &self,
        safety: oink_input::session_gate::InputSafety,
        item: InjectItem,
        mode_sequence: u64,
        source_time: Option<oink_input::source_time::CaptureTime>,
        report: Option<crate::telemetry::Stamp>,
    ) {
        self.posting.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let _posting = Posting(&self.posting);
        if self.suspended.load(std::sync::atomic::Ordering::Acquire) && self.stopped() {
            self.complete_dormant(item);
            return;
        }

        let queued_at = std::time::Instant::now();
        let report = report.or_else(|| {
            if matches!(
                &item,
                InjectItem::Dnd {
                    command: oink_routing::DndCmd::Begin { .. },
                    ..
                }
            ) {
                crate::telemetry::producer().and_then(|p| p.stamp_at(queued_at))
            } else {
                None
            }
        });
        if matches!(
            item,
            InjectItem::Inject {
                event: InjectEvent::MoveTo { .. }
                    | InjectEvent::MoveToWithDelta { .. }
                    | InjectEvent::MoveRelative { .. },
                ..
            }
        ) && self.discard_source_motion()
        {
            return; // Only newly arriving pointer motion is discarded; no replay queue.
        }
        let first_motion = matches!(
            item,
            InjectItem::Inject {
                event: InjectEvent::MoveTo { .. }
                    | InjectEvent::MoveToWithDelta { .. }
                    | InjectEvent::MoveRelative { .. },
                ..
            }
        )
        .then(|| self.first_motion_epoch.swap(0, std::sync::atomic::Ordering::Relaxed))
        .filter(|e| *e != 0);
        let epoch = first_motion
            .or_else(|| {
                matches!(item, InjectItem::SetMode(_) | InjectItem::RestoreLocal { .. })
                    .then(|| self.handoff_epoch.load(std::sync::atomic::Ordering::Relaxed))
            })
            .unwrap_or(0);
        assert!(
            self.queue
                .push(Queued {
                    safety,
                    crossing: (epoch != 0).then_some(Token { epoch, sequence: 0 }),
                    first_motion: first_motion.is_some(),
                    item,
                    at: queued_at,
                    _pending: self.timing.pending(queued_at),
                    mode_sequence,
                    source_time,
                    report,
                })
                .is_ok(),
            "input thread exceeded injection reservation"
        );
        if self.gate.notify() {
            self.thread.lock().unwrap_or_else(|e| e.into_inner()).unpark();
        }
    }
    /// Restore main's callback-time transition. Never wait for a mode call on the
    /// injector: it may itself be waiting for this native callback to return.
    /// The shared sequence only serializes native mode calls, not router state.
    pub(crate) fn mode_in_callback(&self, safety: oink_input::session_gate::InputSafety, item: InjectItem) {
        if !self.backend.input_safety().accepts(safety) {
            return;
        }
        let epoch = self.handoff_epoch.load(std::sync::atomic::Ordering::Relaxed);
        let _scope = handoff_timing::enter_scope((epoch != 0).then_some(Token { epoch, sequence: 0 }));
        assert!(matches!(item, InjectItem::SetMode(_) | InjectItem::RestoreLocal { .. }));
        let sequence = self.next_mode.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        self.record_mode_request(&item, sequence);
        match self.mode_applied.try_lock() {
            Ok(mut applied) => {
                if apply_mode(
                    self.backend.as_ref(),
                    &item,
                    sequence,
                    &mut applied,
                    self.trace.as_deref(),
                    &self.faults,
                ) {
                    trace_mode_route(&item, sequence, 0, Some(true));
                    return;
                }
                trace_mode_route(&item, sequence, 5, Some(false));
            }
            Err(_) => trace_mode_route(&item, sequence, 1, Some(false)),
        }
        self.enqueue_reported(safety, item, sequence, None, None);
    }
    pub(crate) fn timing(&self) -> std::sync::Arc<oink_input::timing::InjectionTiming> {
        self.timing.clone()
    }
    pub fn available(&self) -> usize {
        self.queue.capacity() - self.queue.len()
    }
    pub fn scheduler_status(&self) -> std::sync::Arc<oink_network::link::SchedulerStatus> {
        self.scheduler.load()
    }
    pub(crate) fn scheduler_snapshot(
        &self,
    ) -> std::sync::Arc<oink_foundation::snapshot::Snapshot<oink_network::link::SchedulerStatus>> {
        self.scheduler.clone()
    }
    pub fn backend(&self) -> std::sync::Arc<dyn oink_input::InputBackend> {
        self.backend.clone()
    }
    pub(crate) fn set_wake(&self, wake: oink_network::link::Wake) {
        let _ = self.wake.set(wake);
    }
    pub(crate) fn take_released(&self) -> Option<DeviceId> {
        self.released.pop()
    }
    pub(crate) fn stop(&self) {
        if !self.stopping.swap(true, std::sync::atomic::Ordering::AcqRel) {
            self.push(InjectItem::Stop);
        }
    }
    pub(crate) fn stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::Acquire)
    }
}

// Trace only mode boundaries, never keys or injected payloads. Reasons: 0 callback,
// 1 callback deferred, 2 ordinary queue, 3 queued apply, 4 stale queued mode skipped, 5 callback native error.
fn trace_mode_route(item: &InjectItem, sequence: u64, reason: u32, success: Option<bool>) {
    use oink_input::native_lifecycle::{self, Data, Kind};
    if sequence == 0 || !native_lifecycle::lifecycle_enabled() {
        return;
    }
    let (remote, point) = match *item {
        InjectItem::SetMode(CaptureMode::Remote { park }) => (true, Some(park)),
        InjectItem::SetMode(CaptureMode::Local) => (false, None),
        InjectItem::RestoreLocal { at } => (false, Some(at)),
        _ => return,
    };
    native_lifecycle::emit(
        Kind::ModeRoute,
        Data {
            remote: Some(remote),
            point: point.map(|p| [p.x.round() as i32, p.y.round() as i32]),
            extra: sequence,
            reason,
            success,
            ..Data::default()
        },
    );
}

fn apply_mode(
    native: &dyn oink_input::InputBackend,
    item: &InjectItem,
    sequence: u64,
    applied: &mut u64,
    trace: Option<&crate::diagnostics::input_trace::InputTrace>,
    faults: &super::input::faults::NativeFaults,
) -> bool {
    if sequence <= *applied {
        trace_mode_route(item, sequence, 4, Some(true));
        return true;
    }
    let (mode, restore) = match *item {
        InjectItem::SetMode(mode) => (Some(mode), None),
        InjectItem::RestoreLocal { at } => (Some(CaptureMode::Local), Some(at)),
        _ => unreachable!(),
    };
    if let Some(trace) = trace {
        trace.mode(mode, restore, sequence, true, None);
    }
    let result = match *item {
        InjectItem::SetMode(mode) => native.set_mode(mode),
        InjectItem::RestoreLocal { at } => native.restore_local(at),
        _ => unreachable!("only capture modes carry a sequence"),
    };
    if let Some(trace) = trace {
        trace.mode(mode, restore, sequence, false, Some(result.is_ok()));
    }
    // A mode queued after intentional capture withdrawal needs no native work.
    // The capture owner reports actual required capture failures separately.
    if !matches!(result, Err(oink_input::InputError::NotStarted)) {
        faults.mouse(super::input::faults::native_result(&result));
    }
    if matches!(result, Ok(()) | Err(oink_input::InputError::NotStarted)) {
        *applied = sequence;
        true
    } else {
        false
    }
}
impl Drop for InjectHandle {
    fn drop(&mut self) {
        // Stop has a reserved slot. Only the owning input thread drops this handle.
        self.stop();
        if let Some(thread) = self.join.take() {
            let _ = thread.join();
        }
    }
}

/// Start the thread at `Tier::Realtime`. `released` runs after each `ReleaseAll`.
pub fn start(released: Box<dyn Fn(DeviceId) + Send>) -> std::io::Result<InjectHandle> {
    let backend = oink_input::platform_backend().map_err(std::io::Error::other)?;
    start_with_backend(std::sync::Arc::from(backend), released)
}

/// The same backend is shared with capture, so mode barriers affect the active tap.
/// Backend creation happens on the ordinary startup thread before scheduling is raised.
pub fn start_with_backend(
    backend: std::sync::Arc<dyn oink_input::InputBackend>,
    released: Box<dyn Fn(DeviceId) + Send>,
) -> std::io::Result<InjectHandle> {
    start_with_trace(backend, released, None)
}
pub(crate) fn start_with_trace(
    backend: std::sync::Arc<dyn oink_input::InputBackend>,
    released: Box<dyn Fn(DeviceId) + Send>,
    trace: Option<std::sync::Arc<crate::diagnostics::input_trace::InputTrace>>,
) -> std::io::Result<InjectHandle> {
    let timing = oink_input::timing::InjectionTiming::new(CAPACITY);
    let measured = timing.clone();
    let queue = std::sync::Arc::new(crossbeam_queue::ArrayQueue::<Queued>::new(CAPACITY));
    let input = queue.clone();
    let gate = std::sync::Arc::new(oink_foundation::wake::WakeGate::default());
    let parked = gate.clone();
    let scheduler =
        std::sync::Arc::new(oink_foundation::snapshot::Snapshot::<oink_network::link::SchedulerStatus>::default());
    let scheduling = scheduler.clone();
    let completions = std::sync::Arc::new(crossbeam_queue::ArrayQueue::new(32));
    let released_queue = completions.clone();
    let wake = std::sync::Arc::new(std::sync::OnceLock::<oink_network::link::Wake>::new());
    let input_wake = wake.clone();
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped_flag = stopped.clone();
    let mode_applied = std::sync::Arc::new(std::sync::Mutex::new(0));
    let applied_mode = mode_applied.clone();
    let native = backend.clone();
    let source_starts = std::sync::Arc::new(crossbeam_queue::ArrayQueue::<SourceRequest>::new(1));
    let start_requests = source_starts.clone();
    let source_motion = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let motion_drop = source_motion.clone();
    let input_trace = trace.clone();
    let failures = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let failed = failures.clone();
    let faults = std::sync::Arc::new(super::input::faults::NativeFaults::default());
    let native_faults = faults.clone();
    let suspended = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let suspend_request = suspended.clone();
    let released_callback = std::sync::Arc::new(std::sync::Mutex::new(released));
    let callback = released_callback.clone();
    let posting = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let producers = posting.clone();
    let backlog_trigger = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let backlog = backlog_trigger.clone();
    let ordinary_age = oink_foundation::pending_age::PendingAge::new(CAPACITY);
    let ordinary = ordinary_age.clone();
    let spawn = Box::new(move || {
        let measured = measured.clone();
        let input = input.clone();
        let parked = parked.clone();
        let scheduling = scheduling.clone();
        let released_queue = released_queue.clone();
        let input_wake = input_wake.clone();
        let stopped_flag = stopped_flag.clone();
        let applied_mode = applied_mode.clone();
        let native = native.clone();
        let start_requests = start_requests.clone();
        let motion_drop = motion_drop.clone();
        let input_trace = input_trace.clone();
        let failed = failed.clone();
        let native_faults = native_faults.clone();
        let suspend_request = suspend_request.clone();
        let released = callback.clone();
        let producers = producers.clone();
        let backlog = backlog.clone();
        let ordinary = ordinary.clone();
        std::thread::Builder::new().name("oink-inject".into()).spawn(move || {
            if let Err(error) = native.start_injection_session() {
                tracing::error!(%error, "injection session initialization failed");
                native_faults.screen(super::input::faults::native_result(&Err(error)));
                failed.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }

            let _quiet = tracing::subscriber::set_default(tracing::subscriber::NoSubscriber::default());
            let applied = oink_foundation::realtime::apply(
                oink_foundation::realtime::ThreadRole::Inject,
                oink_foundation::realtime::Tier::Realtime,
            )
            .ok()
            .or_else(|| oink_foundation::realtime::read_back().ok());
            let mut status = oink_network::link::SchedulerStatus {
                applied,
                ..Default::default()
            };
            scheduling.publish(status.clone());
            let mut read_back_at = std::time::Instant::now() + oink_foundation::realtime::READBACK_INTERVAL;
            let mut held = Held::default();
            let mut dnd_pending: Option<(std::sync::Arc<std::sync::atomic::AtomicBool>, bool)> = None;
            let mut deferred: Option<Queued> = None;
            let mut source_pending: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;
            let mut rebase_motion = false;
            let mut safety = native.input_safety();
            let mut pause_cleaned = false;
            let safety_wake: oink_input::capture_loop::Wake = {
                let thread = std::thread::current();
                let gate = parked.clone();
                std::sync::Arc::new(move || {
                    if gate.notify() {
                        thread.unpark();
                    }
                })
            };
            native.watch_input_safety(&safety_wake);
            loop {
                parked.begin_turn();
                let current = native.input_safety();
                if current.epoch != safety.epoch
                    || (!current.active() && safety.active())
                    || (suspend_request.load(std::sync::atomic::Ordering::Acquire) && !pause_cleaned)
                {
                    // Pause is out of band: it cannot sit behind a native call,
                    // DnD completion or queued motion. An in-flight OS call cannot be
                    // cancelled, but no subsequent old-epoch work may be replayed.
                    for owner in held.0.keys().copied().collect::<Vec<_>>() {
                        held.release(owner, native.as_ref());
                    }
                    let _ = native.set_relative_capture(false);
                    let _ = native.set_mode(CaptureMode::Local);
                    dnd_pending = None;
                    source_pending = None;
                    motion_drop.store(0, std::sync::atomic::Ordering::Release);
                    rebase_motion = false;
                    pause_cleaned = suspend_request.load(std::sync::atomic::Ordering::Acquire);
                    if pause_cleaned {
                        native.stop_injection_session();
                    }
                    if let Some(wake) = input_wake.get() {
                        wake();
                    }
                }
                if current.active()
                    && current != safety
                    && native_faults.snapshot().screen
                        == Some(oink_protocol::v1::dto::ResourceFaultCode::PermissionDenied)
                {
                    // The old admission fault must not prevent the new session from
                    // starting; otherwise no fresh injection can ever clear it.
                    native_faults.screen(oink_protocol::v1::dto::ResourceFaultCode::None);
                    if let Some(wake) = input_wake.get() {
                        wake();
                    }
                }
                safety = current;
                if pause_cleaned {
                    // Cleanup is complete before dormant control acknowledgments
                    // become visible. Wait for any producer that saw the old state
                    // to publish before deciding the queue is finally empty.
                    stopped_flag.store(true, std::sync::atomic::Ordering::Release);
                }
                let now = std::time::Instant::now();
                if now >= read_back_at {
                    if oink_foundation::realtime::read_back().ok() != applied {
                        status.demoted += 1;
                    }
                    scheduling.publish(status.clone());
                    read_back_at = now + oink_foundation::realtime::READBACK_INTERVAL;
                }
                // A continuation of the current Begin, not a new ordered command:
                // Up/Cancel can already be deferred waiting for that same Begin.
                // Admit its short press section even in that case, without moving
                // either boundary or any queued motion. Native work stays ordinary.
                if source_pending
                    .as_ref()
                    .is_some_and(|ready| ready.load(std::sync::atomic::Ordering::Acquire))
                {
                    source_pending = None;
                    rebase_motion = true;
                }
                if source_pending.is_none()
                    && let Some(request) = start_requests.pop()
                {
                    if !safety.accepts(request.work.safety)
                        || suspend_request.load(std::sync::atomic::Ordering::Acquire)
                    {
                        drop(request);
                        continue;
                    }
                    let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    source_pending = Some(ready.clone());
                    motion_drop.store(SOURCE_ACTIVE, std::sync::atomic::Ordering::Release);
                    let _ = request.reply.send(DndCompletion {
                        work: Some(request.work),
                        ready,
                        thread: std::thread::current(),
                        gate: parked.clone(),
                        source_motion: Some(motion_drop.clone()),
                    });
                }
                if source_pending.is_some() {
                    if parked.prepare_wait() {
                        std::thread::park_timeout(read_back_at.saturating_duration_since(now));
                    }
                    continue;
                }
                if dnd_pending
                    .as_ref()
                    .is_some_and(|(ready, _)| ready.load(std::sync::atomic::Ordering::Acquire))
                {
                    dnd_pending = None;
                }
                let Some(queued) = deferred.take().or_else(|| input.pop()) else {
                    if suspend_request.load(std::sync::atomic::Ordering::Acquire) {
                        if producers.load(std::sync::atomic::Ordering::Acquire) != 0 {
                            std::thread::yield_now();
                            continue;
                        }
                        break;
                    }
                    if parked.prepare_wait() {
                        std::thread::park_timeout(read_back_at.saturating_duration_since(now));
                    }
                    continue;
                };
                if now.saturating_duration_since(queued.at) > std::time::Duration::from_secs(5) && safety.active() {
                    native.set_input_paused(true);
                    suspend_request.store(true, std::sync::atomic::Ordering::Release);
                    backlog.store(true, std::sync::atomic::Ordering::Release);
                    if let InjectItem::OwnerFence { state, .. } | InjectItem::UpdateFence(state) = queued.item {
                        state.store(2, std::sync::atomic::Ordering::Release);
                    }
                    while let Some(old) = input.pop() {
                        if let InjectItem::OwnerFence { state, .. } | InjectItem::UpdateFence(state) = old.item {
                            state.store(2, std::sync::atomic::Ordering::Release);
                        }
                    }
                    if let Some(wake) = input_wake.get() {
                        wake();
                    }
                    continue;
                }
                if (!native.input_safety().accepts(queued.safety)
                    || suspend_request.load(std::sync::atomic::Ordering::Acquire))
                    && session_sensitive(&queued.item)
                {
                    // Keep fences, release acknowledgements and drag cancellation.
                    // Dropping a command retires its pending timing accounting too.
                    if input.len() == CAPACITY / 2
                        && let Some(wake) = input_wake.get()
                    {
                        wake();
                    }
                    continue;
                }
                // Native drag preparation (e.g. File Provider/AppKit startup) may
                // take hundreds of milliseconds. Absolute motion must keep the
                // cursor current instead of freezing it and replaying a burst
                // afterwards. Preserve every other queue
                // boundary (especially Up, Cancel and capture modes), including
                // motion behind that boundary. Windows routes ordinary motion as
                // MoveToWithDelta too; its delta is capture evidence, not a barrier.
                // One deferred head retains bounded backpressure; the ordinary worker
                // wakes us on completion.
                if let Some((_, motion_allowed)) = &dnd_pending
                    && !(*motion_allowed
                        && matches!(
                            queued.item,
                            InjectItem::Inject {
                                event: InjectEvent::MoveTo { .. } | InjectEvent::MoveToWithDelta { .. },
                                ..
                            }
                        ))
                {
                    deferred = Some(queued);
                    if parked.prepare_wait() {
                        std::thread::park_timeout(read_back_at.saturating_duration_since(now));
                    }
                    continue;
                }
                let started = now;
                let waited = started.saturating_duration_since(queued.at);
                measured.waited(waited.as_micros() as u64);
                if waited > oink_foundation::realtime::ThreadRole::Inject.budget().constraint {
                    status.late_rounds += 1;
                    status.late_max = status.late_max.max(waited);
                }
                let _scope = handoff_timing::enter_scope(queued.crossing);
                match queued.item {
                    InjectItem::Inject { mut event, from } => {
                        if queued.first_motion {
                            handoff_timing::native(Stage::FirstMotionStarted, 0);
                        }
                        let source = from
                            .zip(queued.source_time)
                            .map(|(origin, time)| oink_input::source_time::InjectionSource { origin, time });
                        let class = oink_input::timing::CallClass::event(&event);
                        let called = std::time::Instant::now();
                        measured.native_begin(class, called);
                        if rebase_motion
                            && matches!(
                                event,
                                InjectEvent::MoveTo { .. }
                                    | InjectEvent::MoveToWithDelta { .. }
                                    | InjectEvent::MoveRelative { .. }
                            )
                        {
                            rebase_motion = false;
                            let current = native.rebase_pointer_motion();
                            if let InjectEvent::MoveTo { pos } | InjectEvent::MoveToWithDelta { pos, .. } = event {
                                let current = current.unwrap_or(pos);
                                event = InjectEvent::MoveToWithDelta {
                                    pos,
                                    dx: pos.x - current.x,
                                    dy: pos.y - current.y,
                                };
                            }
                            // Relative injection already starts at the native cursor on
                            // both platforms; discarded events never reach its accumulator.
                        }
                        let result = oink_input::source_time::with_injection_source(source, || {
                            if matches!(event, InjectEvent::Gesture(_))
                                && input_trace.as_ref().is_some_and(|t| t.enabled())
                            {
                                oink_input::gesture_trace::with_source(from, || native.inject(&event))
                            } else {
                                native.inject(&event)
                            }
                        });
                        let success = result.is_ok();
                        if queued.first_motion {
                            handoff_timing::native(Stage::FirstMotionCompleted, u32::from(!success));
                        }
                        let completed_at = std::time::Instant::now();
                        measured.native_end(class, completed_at.saturating_duration_since(called).as_micros() as u64);
                        if let (Some(stamp), Some(p)) = (queued.report, crate::telemetry::producer()) {
                            p.interval(
                                stamp,
                                oink_telemetry::Metric::InputReceiveToNativeComplete,
                                completed_at,
                            );
                        }
                        if success {
                            held.record(from, &event);
                        } else {
                            // Detection only: a local hook may already have swallowed
                            // the physical event. No retry/fallback can restore that
                            // original delivery. Never record the key or native payload.
                            failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        // A successful move cannot clear a failed key/button cleanup.
                        let cleanup_pending = native_faults.snapshot().screen
                            == Some(oink_protocol::v1::dto::ResourceFaultCode::CleanupFailed)
                            && !held.0.is_empty();
                        if !cleanup_pending
                            && native_faults.screen(super::input::faults::native_result(&result))
                            && let Some(wake) = input_wake.get()
                        {
                            wake();
                        }
                        if let Some(trace) = &input_trace {
                            trace.injected(&event, from, success);
                        }
                    }
                    item @ (InjectItem::SetMode(_) | InjectItem::RestoreLocal { .. }) => {
                        let mut applied = applied_mode.lock().unwrap_or_else(|e| e.into_inner());
                        trace_mode_route(&item, queued.mode_sequence, 3, None);
                        let called = std::time::Instant::now();
                        measured.native_begin(oink_input::timing::CallClass::Mode, called);
                        let success = apply_mode(
                            native.as_ref(),
                            &item,
                            queued.mode_sequence,
                            &mut applied,
                            input_trace.as_deref(),
                            &native_faults,
                        );
                        if !success {
                            failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        if let Some(wake) = input_wake.get() {
                            wake();
                        }
                        measured.native_end(oink_input::timing::CallClass::Mode, called.elapsed().as_micros() as u64);
                    }
                    InjectItem::SetRelativeCapture(active) => {
                        let called = std::time::Instant::now();
                        measured.native_begin(oink_input::timing::CallClass::Mode, called);
                        let result = native.set_relative_capture(active);
                        if result.is_err() {
                            failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        if native_faults.mouse(super::input::faults::native_result(&result))
                            && let Some(wake) = input_wake.get()
                        {
                            wake();
                        }
                        measured.native_end(oink_input::timing::CallClass::Mode, called.elapsed().as_micros() as u64);
                    }
                    InjectItem::ReleaseAll { peer } => {
                        // Router revocation queued its native Ups before this barrier.
                        // A shared modifier may still be held by another peer; issuing
                        // another Up from the first injector's label would release it.
                        // The physical ledger is cleared only by actual Up events.
                        assert!(released_queue.push(peer).is_ok(), "one release per detached peer");
                        if let Some(wake) = input_wake.get() {
                            wake();
                        }
                        (released.lock().unwrap_or_else(|e| e.into_inner()))(peer);
                    }
                    InjectItem::Dnd { command, main } => {
                        if let (Some(stamp), Some(p)) = (queued.report, crate::telemetry::producer().as_ref()) {
                            p.interval(stamp, oink_telemetry::Metric::DragQueueWait, started);
                        }
                        if let Some(trace) = &input_trace {
                            trace.dnd_command(&command, "CommandDispatched", Some(waited.as_micros() as u64), None);
                        }
                        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let motion_allowed = matches!(command, oink_routing::DndCmd::Begin { .. });
                        dnd_pending = Some((ready.clone(), motion_allowed));
                        ordinary.discard_before(queued.safety.epoch);
                        let Some(pending) = ordinary.track_epoch(queued.at, queued.safety.epoch) else {
                            continue;
                        };
                        let completion = DndCompletion {
                            work: Some(OrdinaryInput {
                                safety: queued.safety,
                                at: queued.at,
                                _pending: pending,
                            }),
                            ready,
                            thread: std::thread::current(),
                            gate: parked.clone(),
                            source_motion: None,
                        };
                        let _ = main.send(super::MainCommand::Dnd {
                            safety: queued.safety,
                            command,
                            completion,
                            report: queued.report,
                        });
                    }
                    InjectItem::OwnerBegin(baseline) => {
                        baseline.store(
                            failed.load(std::sync::atomic::Ordering::Acquire),
                            std::sync::atomic::Ordering::Release,
                        );
                    }
                    InjectItem::OwnerFence {
                        state,
                        failures,
                        expect_released,
                    } => {
                        let cleanup_pending = expect_released && !held.0.is_empty();
                        if cleanup_pending {
                            native_faults.screen(oink_protocol::v1::dto::ResourceFaultCode::CleanupFailed);
                        }
                        let result = if cleanup_pending {
                            3
                        } else if failed.load(std::sync::atomic::Ordering::Acquire)
                            != failures.load(std::sync::atomic::Ordering::Acquire)
                        {
                            2
                        } else {
                            1
                        };
                        state.store(result, std::sync::atomic::Ordering::Release);
                        if let Some(wake) = input_wake.get() {
                            wake();
                        }
                    }
                    InjectItem::UpdateFence(fence) => {
                        // The router refuses preparation while any held key/button
                        // exists. Recheck the injection ledger after queued work.
                        fence.store(
                            if held.0.is_empty() { 1 } else { 2 },
                            std::sync::atomic::Ordering::Release,
                        );
                    }
                    InjectItem::Stop => {
                        for owner in held.0.keys().copied().collect::<Vec<_>>() {
                            held.release(owner, native.as_ref());
                        }
                        break;
                    }
                }
                let elapsed = started.elapsed();
                if elapsed > oink_foundation::realtime::ThreadRole::Inject.budget().computation {
                    status.over_budget += 1;
                    status.over_budget_max = status.over_budget_max.max(elapsed);
                }
                if input.len() == CAPACITY / 2
                    && let Some(wake) = input_wake.get()
                {
                    wake();
                }
            }
            if !pause_cleaned {
                native.stop_injection_session();
            }
            stopped_flag.store(true, std::sync::atomic::Ordering::Release);
            if let Some(wake) = input_wake.get() {
                wake();
            }
        })
    });
    let join = spawn()?;
    Ok(InjectHandle {
        trace,
        queue,
        gate,
        scheduler,
        thread: std::sync::Arc::new(std::sync::Mutex::new(join.thread().clone())),
        join: Some(join),
        spawn,
        suspended,
        released_callback,
        posting,
        backlog_trigger,
        ordinary_age,
        backend,
        released: completions,
        wake,
        stopping: std::sync::atomic::AtomicBool::new(false),
        stopped,
        next_mode: std::sync::atomic::AtomicU64::new(0),
        failures,
        faults,
        mode_applied,
        source_starts,
        source_motion,
        timing,
        handoff_epoch: std::sync::atomic::AtomicU64::new(0),
        first_motion_epoch: std::sync::atomic::AtomicU64::new(0),
    })
}

#[derive(Default)]
struct Held(std::collections::HashMap<Option<DeviceId>, Vec<InjectEvent>>);
impl Held {
    fn record(&mut self, owner: Option<DeviceId>, event: &InjectEvent) {
        let (release, down) = match event {
            InjectEvent::Key { code, pressed } => (
                InjectEvent::Key {
                    code: *code,
                    pressed: false,
                },
                *pressed,
            ),
            InjectEvent::Button { button, pressed } => (
                InjectEvent::Button {
                    button: *button,
                    pressed: false,
                },
                *pressed,
            ),
            _ => return,
        };
        let held = self.0.entry(owner).or_default();
        if down {
            if !held.contains(&release) {
                held.push(release);
            }
        } else {
            // Native state is aggregate: an Up from the final routing contributor
            // clears the physical press, even if another peer caused its initial Down.
            self.0.retain(|_, held| {
                held.retain(|event| *event != release);
                !held.is_empty()
            });
        }
    }
    fn release(&mut self, owner: Option<DeviceId>, native: &dyn oink_input::InputBackend) {
        if let Some(events) = self.0.remove(&owner) {
            for event in events {
                let _ = native.inject(&event);
            }
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    #[test]
    fn pause_discards_queued_input_releases_held_once_and_joins_before_fresh_session() {
        use super::*;
        use oink_input::InputBackend;
        use std::sync::{Arc, mpsc};
        use std::time::{Duration, Instant};
        let backend = Arc::new(oink_test_support::input::FakeInput::new());
        let (entered, waiting) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let count = std::sync::atomic::AtomicUsize::new(0);
        let mut handle = start_with_backend(
            backend.clone(),
            Box::new(move |_| {
                entered.send(()).unwrap();
                if count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                    resumed.recv().unwrap();
                }
            }),
        )
        .unwrap();
        let peer = DeviceId([7; 32]);
        let down = InjectEvent::Key {
            code: oink_input::KeyCode::LEFT_SHIFT,
            pressed: true,
        };
        let up = InjectEvent::Key {
            code: oink_input::KeyCode::LEFT_SHIFT,
            pressed: false,
        };
        handle.push(InjectItem::Inject {
            event: down.clone(),
            from: Some(peer),
        });
        handle.push(InjectItem::ReleaseAll { peer });
        waiting.recv_timeout(Duration::from_secs(3)).unwrap();
        handle.push(InjectItem::Inject {
            event: InjectEvent::MoveTo {
                pos: Point::new(800., 400.),
            },
            from: Some(peer),
        });
        backend.set_input_paused(true);
        handle.suspend();
        assert!(!handle.detached(), "in-flight cleanup must remain visible");
        resume.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while !handle.detached() {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert!(!backend.injection_active());
        assert_eq!(backend.take_injected(), vec![down, up]);
        handle.push(InjectItem::Inject {
            event: InjectEvent::MoveRelative { dx: 10., dy: 0. },
            from: Some(peer),
        });
        assert!(backend.take_injected().is_empty());
        assert_eq!(backend.injection_sessions(), 1);
        backend.set_input_paused(false);
        assert!(handle.resume().unwrap());
        let fresh = InjectEvent::MoveTo {
            pos: Point::new(50., 60.),
        };
        handle.push(InjectItem::Inject {
            event: fresh.clone(),
            from: Some(peer),
        });
        handle.push(InjectItem::ReleaseAll { peer });
        waiting.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(backend.injection_sessions(), 2);
        assert_eq!(backend.take_injected(), vec![fresh]);
    }

    #[test]
    fn pause_discards_backlog_even_if_resumed_before_native_call_returns() {
        use super::*;
        use oink_input::InputBackend;
        use std::sync::{Arc, mpsc};
        use std::time::Duration;
        for down in [
            InjectEvent::Key {
                code: oink_input::KeyCode::LEFT_SHIFT,
                pressed: true,
            },
            InjectEvent::Button {
                button: oink_input::MouseButton::Left,
                pressed: true,
            },
        ] {
            let backend = Arc::new(oink_test_support::input::FakeInput::new());
            backend.start(Arc::new(|_| oink_input::Disposition::Pass)).unwrap();
            let (entered, waiting) = mpsc::channel();
            let (resume, resumed) = mpsc::channel();
            // A native completion blocked in flight while the session pauses.
            let count = std::sync::atomic::AtomicUsize::new(0);
            let handle = start_with_backend(
                backend.clone(),
                Box::new(move |_| {
                    entered.send(()).unwrap();
                    if count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                        resumed.recv().unwrap();
                    }
                }),
            )
            .unwrap();
            let peer = DeviceId([7; 32]);
            handle.push(InjectItem::Inject {
                event: down.clone(),
                from: Some(peer),
            });
            handle.push(InjectItem::ReleaseAll { peer });
            waiting.recv_timeout(Duration::from_secs(3)).unwrap();
            for item in [
                InjectItem::Inject {
                    event: InjectEvent::MoveTo {
                        pos: Point::new(800., 400.),
                    },
                    from: Some(peer),
                },
                InjectItem::Inject {
                    event: InjectEvent::Key {
                        code: oink_input::KeyCode::keyboard(4),
                        pressed: true,
                    },
                    from: Some(peer),
                },
                InjectItem::SetMode(CaptureMode::Remote {
                    park: Point::new(800., 400.),
                }),
            ] {
                handle.push(item);
            }
            let routed_before_pause = backend.input_safety();
            backend.set_input_paused(true);
            backend.set_input_paused(false);
            // Routing started before pause, but its output arrives only
            // after resume. Enqueue must retain its original generation.
            handle.push_in_epoch(
                routed_before_pause,
                InjectItem::Inject {
                    event: InjectEvent::Key {
                        code: oink_input::KeyCode::keyboard(5),
                        pressed: true,
                    },
                    from: Some(peer),
                },
            );
            let fresh = InjectEvent::MoveTo {
                pos: Point::new(50., 60.),
            };
            handle.push(InjectItem::Inject {
                event: fresh.clone(),
                from: Some(peer),
            });
            handle.push(InjectItem::ReleaseAll { peer });
            resume.send(()).unwrap();
            waiting.recv_timeout(Duration::from_secs(3)).unwrap();
            let up = match down.clone() {
                InjectEvent::Key { code, .. } => InjectEvent::Key { code, pressed: false },
                InjectEvent::Button { button, .. } => InjectEvent::Button { button, pressed: false },
                _ => unreachable!(),
            };
            assert_eq!(backend.take_injected(), vec![down, up, fresh]);
            assert_eq!(backend.mode(), Some(CaptureMode::Local));
            drop(handle);
        }
    }

    #[test]
    fn pause_wakes_idle_injector_and_releases_held_input() {
        use super::*;
        use oink_input::InputBackend;
        use std::sync::{Arc, mpsc};
        use std::time::{Duration, Instant};
        let backend = Arc::new(oink_test_support::input::FakeInput::new());
        let (done, received) = mpsc::channel();
        let handle = start_with_backend(
            backend.clone(),
            Box::new(move |_| {
                done.send(()).unwrap();
            }),
        )
        .unwrap();
        let peer = DeviceId([7; 32]);
        handle.push(InjectItem::Inject {
            from: Some(peer),
            event: InjectEvent::Key {
                code: oink_input::KeyCode::LEFT_SHIFT,
                pressed: true,
            },
        });
        handle.push(InjectItem::ReleaseAll { peer });
        received.recv_timeout(Duration::from_secs(3)).unwrap();
        backend.set_input_paused(true);
        let end = Instant::now() + Duration::from_secs(2);
        while backend.injected().len() < 2 && Instant::now() < end {
            std::thread::yield_now();
        }
        assert_eq!(
            backend.injected().len(),
            2,
            "pause must not wait for another remote event"
        );
        drop(handle);
    }
    #[test]
    fn native_owner_exposes_source_interval_and_clears_it_for_cleanup() {
        use super::*;
        use oink_input::source_time::{CaptureTime, InjectionSource};
        let backend = std::sync::Arc::new(oink_test_support::input::FakeInput::new());
        backend.record_injection_sources();
        let (released, release) = std::sync::mpsc::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        let handle = start_with_backend(
            backend.clone(),
            Box::new(move |_| {
                released.send(()).unwrap();
                resumed.recv().unwrap();
            }),
        )
        .unwrap();
        let origin = DeviceId([3; 32]);
        for (pressed, time) in [(true, 10), (false, 20), (true, 300_010)] {
            handle.push_timed(
                InjectItem::Inject {
                    event: InjectEvent::Button {
                        button: oink_input::MouseButton::Left,
                        pressed,
                    },
                    from: Some(origin),
                },
                Some(CaptureTime(time)),
            );
        }
        handle.push(InjectItem::ReleaseAll { peer: origin });
        handle.stop();
        release.recv().unwrap();
        // ReleaseAll acknowledges revocation, not shutdown cleanup. Force that
        // boundary before Stop, then join before inspecting its final native Up.
        assert!(!handle.stopped());
        resume.send(()).unwrap();
        drop(handle);
        let sources = backend.take_injection_sources();
        assert_eq!(
            sources,
            vec![
                Some(InjectionSource {
                    origin,
                    time: CaptureTime(10)
                }),
                Some(InjectionSource {
                    origin,
                    time: CaptureTime(20)
                }),
                Some(InjectionSource {
                    origin,
                    time: CaptureTime(300_010)
                }),
                None,
            ]
        );
        assert_eq!(
            sources[2].unwrap().time.elapsed_since(sources[0].unwrap().time),
            Some(300_000)
        );
    }
    #[test]
    fn source_press_discards_only_motion_and_rebases_the_first_position() {
        use oink_input::InputBackend;
        let backend = std::sync::Arc::new(oink_test_support::input::FakeInput::new());
        let handle = start_with_backend(backend.clone(), Box::new(|_| {})).unwrap();
        let (main, commands) = super::super::main_channel();
        handle.push(InjectItem::Dnd {
            command: oink_routing::DndCmd::Begin {
                grant: crate::dnd::test_target(
                    oink_foundation::content::ClipId {
                        origin: DeviceId([1; 32]),
                        seq: 1,
                    },
                    1,
                ),
                epoch: 1,
                id: oink_foundation::content::ClipId {
                    origin: DeviceId([1; 32]),
                    seq: 1,
                },
                at: Point::default(),
                items: Vec::new(),
                retained_inventory: None,
            },
            main,
        });
        let begin = commands.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        let initial = Point::new(40., 50.);
        handle.push(InjectItem::Inject {
            event: InjectEvent::MoveTo { pos: initial },
            from: None,
        });
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while backend.cursor_position().unwrap() != initial {
            assert!(std::time::Instant::now() < limit);
            std::thread::yield_now();
        }
        backend.take_injected();
        let start = handle.source_start();
        let (placed, at_window) = std::sync::mpsc::channel();
        let (release, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            start(&mut || {
                placed.send(()).unwrap();
                receive.recv().unwrap();
            })
            .unwrap()
        });
        at_window.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        for event in [
            InjectEvent::MoveTo {
                pos: Point::new(200., 200.),
            },
            InjectEvent::MoveToWithDelta {
                pos: Point::new(300., 300.),
                dx: 260.,
                dy: 250.,
            },
            InjectEvent::MoveRelative { dx: 400., dy: 400. },
        ] {
            handle.push(InjectItem::Inject { event, from: None });
        }
        let up = InjectEvent::Button {
            button: oink_foundation::input::MouseButton::Left,
            pressed: false,
        };
        let key = InjectEvent::Key {
            code: oink_foundation::input::KeyCode::keyboard(4),
            pressed: true,
        };
        handle.push(InjectItem::Inject {
            event: key.clone(),
            from: None,
        });
        handle.push(InjectItem::Inject {
            event: up.clone(),
            from: None,
        });
        handle.push(InjectItem::SetMode(CaptureMode::Local));
        assert!(backend.injected().is_empty());
        release.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), 3);
        assert!(backend.injected().is_empty(), "Up must still wait for Begin");
        drop(begin);
        let next = Point::new(45., 57.);
        handle.push(InjectItem::Inject {
            event: InjectEvent::MoveToWithDelta {
                pos: next,
                dx: 265.,
                dy: 257.,
            },
            from: None,
        });
        while backend.cursor_position().unwrap() != next {
            assert!(std::time::Instant::now() < limit);
            std::thread::yield_now();
        }
        assert_eq!(
            backend.take_injected(),
            vec![
                key,
                up,
                InjectEvent::MoveToWithDelta {
                    pos: next,
                    dx: 5.,
                    dy: 7.
                }
            ]
        );
    }
    use super::*;
    use oink_foundation::input::KeyCode;

    #[test]
    fn begin_delivers_both_absolute_motion_forms_before_completion_and_preserves_up() {
        use oink_foundation::input::MouseButton;
        let backend = std::sync::Arc::new(oink_test_support::input::FakeInput::new());
        let peer = DeviceId([1; 32]);
        let (finished, finish) = std::sync::mpsc::channel();
        let handle = start_with_backend(backend.clone(), Box::new(move |_| finished.send(()).unwrap())).unwrap();
        let (main, commands) = super::super::main_channel();
        handle.push(InjectItem::Dnd {
            command: oink_routing::DndCmd::Begin {
                grant: crate::dnd::test_target(
                    oink_foundation::content::ClipId {
                        origin: DeviceId([1; 32]),
                        seq: 1,
                    },
                    1,
                ),
                epoch: 1,
                id: oink_foundation::content::ClipId { origin: peer, seq: 1 },
                at: Point::new(0., 0.),
                items: Vec::new(),
                retained_inventory: None,
            },
            main,
        });
        let plain = InjectEvent::MoveTo {
            pos: Point::new(1., 0.),
        };
        let with_delta = InjectEvent::MoveToWithDelta {
            pos: Point::new(2., 0.),
            dx: 1.,
            dy: 0.,
        };
        let up = InjectEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        };
        let after_up = InjectEvent::MoveToWithDelta {
            pos: Point::new(3., 0.),
            dx: 1.,
            dy: 0.,
        };
        for event in [&plain, &with_delta, &up, &after_up] {
            handle.push(InjectItem::Inject {
                event: event.clone(),
                from: Some(peer),
            });
        }
        handle.push(InjectItem::ReleaseAll { peer });
        // Retain the actual ordinary-worker completion, without a timed fake Begin.
        let begin = commands.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        assert!(matches!(
            begin,
            super::super::MainCommand::Dnd {
                command: oink_routing::DndCmd::Begin { .. },
                ..
            }
        ));
        let watchdog = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while backend.injected().len() < 2 && std::time::Instant::now() < watchdog {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let during_begin = backend.injected();
        // Release before asserting so a broken motion filter cannot strand teardown.
        drop(begin);
        finish.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        assert_eq!(
            during_begin,
            vec![plain.clone(), with_delta.clone()],
            "absolute motion must arrive before Begin completes; Up remains a barrier"
        );
        assert_eq!(backend.take_injected(), vec![plain, with_delta, up, after_up]);
    }

    #[test]
    fn callback_mode_never_waits_for_an_in_flight_mode_call() {
        use oink_input::InputBackend;
        let backend = std::sync::Arc::new(oink_test_support::input::FakeInput::new());
        backend
            .start(std::sync::Arc::new(|_| oink_input::Disposition::Pass))
            .unwrap();
        let (done, completed) = std::sync::mpsc::channel();
        let handle = start_with_backend(backend.clone(), Box::new(move |_| done.send(()).unwrap())).unwrap();
        let parked = CaptureMode::Remote {
            park: Point::new(500., 500.),
        };
        let busy = handle.mode_applied.lock().unwrap();
        std::thread::scope(|scope| {
            let (returned, receive) = std::sync::mpsc::channel();
            let callback_handle = &handle;
            scope.spawn(move || {
                callback_handle.mode_in_callback(callback_handle.backend.input_safety(), InjectItem::SetMode(parked));
                returned.send(()).unwrap();
            });
            let result = receive.recv_timeout(std::time::Duration::from_secs(1));
            // Release even on failure: a blocking implementation must not hang the test suite.
            drop(busy);
            assert!(
                result.is_ok(),
                "callback waited for a native mode call that may need that callback"
            );
        });
        handle.push(InjectItem::ReleaseAll {
            peer: DeviceId([1; 32]),
        });
        completed.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(backend.mode(), Some(parked));
    }

    #[test]
    fn callback_restore_uses_atomic_restore_at_entry_point() {
        use oink_input::InputBackend;
        let backend = std::sync::Arc::new(oink_test_support::input::FakeInput::new());
        backend
            .start(std::sync::Arc::new(|_| oink_input::Disposition::Pass))
            .unwrap();
        backend
            .set_mode(CaptureMode::Remote {
                park: Point::new(500., 500.),
            })
            .unwrap();
        let handle = start_with_backend(backend.clone(), Box::new(|_| {})).unwrap();
        let at = Point::new(10., 900.);
        handle.mode_in_callback(handle.backend.input_safety(), InjectItem::RestoreLocal { at });
        assert_eq!(backend.mode(), Some(CaptureMode::Local));
        assert_eq!(backend.cursor_position().unwrap(), at);
        assert_eq!(backend.take_injected(), vec![InjectEvent::MoveTo { pos: at }]);
    }

    #[test]
    fn release_barrier_follows_native_consumption_and_does_not_release_another_peer() {
        let backend = std::sync::Arc::new(oink_test_support::input::FakeInput::new());
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = start_with_backend(
            backend.clone(),
            Box::new(move |peer| {
                tx.send(peer).unwrap();
            }),
        )
        .unwrap();
        let a = DeviceId([1; 32]);
        let b = DeviceId([2; 32]);
        for (peer, code) in [(a, 4), (b, 5)] {
            handle.push(InjectItem::Inject {
                from: Some(peer),
                event: InjectEvent::Key {
                    code: KeyCode::keyboard(code),
                    pressed: true,
                },
            });
        }
        handle.push(InjectItem::Inject {
            from: Some(a),
            event: InjectEvent::Key {
                code: KeyCode::keyboard(4),
                pressed: false,
            },
        });
        handle.push(InjectItem::ReleaseAll { peer: a });
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), a);
        let events = backend.take_injected();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[2],
            InjectEvent::Key {
                code: KeyCode::keyboard(4),
                pressed: false
            }
        );
        drop(handle);
        assert_eq!(
            backend.take_injected(),
            vec![InjectEvent::Key {
                code: KeyCode::keyboard(5),
                pressed: false
            }]
        );
    }
}
