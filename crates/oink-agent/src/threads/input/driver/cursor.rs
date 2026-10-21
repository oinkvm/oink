//! Target motion is held as an unconsumed packet until the native FIFO barrier
//! completes. No speculative Router output (especially FocusEnter/DnD) escapes.
use super::*;
use crate::threads::inject::cursor::Snapshot;
use oink_input::cursor_reconciliation::{Decision, Reconciler};
use std::sync::mpsc::{Receiver, TryRecvError};

// Query cost is sub-microsecond in the read-only desktop measurements. Allow
// one 1 ms native-thread budget plus the 2 ms own-commit ambiguity grace. This
// is a routing deadline, not a promise that a stalled OS call can be cancelled.
pub(super) const MAX_WAIT: Duration = Duration::from_millis(3);
type Scope = (
    u64,
    DeviceId,
    bool,
    bool,
    u64,
    u64,
    usize,
    oink_input::session_gate::InputSafety,
);
#[derive(Default)]
pub(super) struct State {
    context: Option<Scope>,
    scope: u64,
    pub revision: u64,
    pub pending: Option<Pending>,
    reconcile: Reconciler,
    diagnostics: crate::diagnostics::cursor_reconciliation::State,
    late: Option<Receiver<Snapshot>>,
}
pub(super) struct Pending {
    peer: DeviceId,
    source: Option<DeviceId>,
    message: Message,
    pub(super) scope: u64,
    revision: u64,
    pub(super) read: Receiver<Snapshot>,
    pub retry: Option<Instant>,
    pub at: Instant,
    time: Option<CaptureTime>,
    report: Option<crate::telemetry::Stamp>,
    pub(super) arrival: Option<(DeviceId, CaptureTime, Instant, bool)>,
}
pub(super) fn motion(message: &Message) -> bool {
    pointer_motion(message, false)
}
pub(super) fn pointer_motion(message: &Message, include_relative: bool) -> bool {
    fn input(msg: &InputMsg, include_relative: bool) -> bool {
        match msg {
            InputMsg::Motion { .. } => true,
            InputMsg::RelativeMotion { .. } => include_relative,
            InputMsg::Forwarded { msg, .. } => input(msg, include_relative),
            _ => false,
        }
    }
    match message {
        Message::Input(msg) | Message::TimedInput { input: msg, .. } => input(msg, include_relative),
        _ => false,
    }
}
impl Driver {
    pub(super) fn cursor_diagnostics(&mut self) {
        self.cursor
            .diagnostics
            .emit(self.micros(), self.inject.trace.as_deref());
    }
    pub(super) fn cursor_invalidate(&mut self) {
        self.cursor.context = None;
    }
    pub(super) fn cursor_scope(&mut self) {
        let router = self.adapter.router();
        let game = router.game_mode();
        let context = (
            game.epoch,
            game.holder,
            game.active,
            router.paused(),
            self.emergency_sequence,
            self.source_modes_revision,
            Arc::as_ptr(&self.layout) as usize,
            self.input_safety,
        );
        if self.cursor.context.as_ref() != Some(&context) {
            self.cursor.context = Some(context);
            self.cursor.scope = self.cursor.scope.wrapping_add(1);
            self.inject.pointer_scope(self.cursor.scope);
        }
    }
    pub(super) fn cursor_inbound(&mut self, peer: DeviceId, message: Message, source: Option<DeviceId>) {
        self.cursor_scope();
        // SourceStart discards motion, including its virtual displacement. If
        // Router advances first, a later read timeout can replay the lost delta.
        if pointer_motion(&message, true) && self.inject.discard_source_motion() {
            return;
        }
        if let Some(late) = self.cursor.late.take() {
            match late.try_recv() {
                Ok(sample) => self.cursor.diagnostics.late(sample.read_us),
                Err(TryRecvError::Empty) => self.cursor.late = Some(late),
                Err(TryRecvError::Disconnected) => {}
            }
            // A late sample predates the timeout's submitted delta. It informs
            // the next read cycle, but its coordinates cannot be adopted without
            // a new FIFO barrier AFTER that delta's native acceptance.
        }
        let router = self.adapter.router();
        if motion(&message)
            && self.adapter.attached(peer)
            && router.has_pointer_focus()
            && !router.paused()
            && !router.game_mode().active
            && !router.local_displays().is_empty()
        {
            // Network reads stop as soon as this slot is occupied. Reliable
            // buttons/keys and later datagrams remain in their bounded queues.
            debug_assert!(self.cursor.pending.is_none());
            self.cursor.pending = Some(Pending {
                peer,
                source,
                message,
                scope: self.cursor.scope,
                revision: self.cursor.revision,
                read: self.inject.read_cursor(self.cursor.scope),
                retry: None,
                at: self.now,
                time: self.input_time,
                report: self.input_report,
                arrival: None,
            });
            return;
        }
        if clip_decision(&message) {
            self.refresh_system_cursor_clip();
        }
        self.adapter.inbound(peer, message);
    }
    pub(super) fn cursor_resume(&mut self) {
        self.cursor_scope();
        let Some(mut pending) = self.cursor.pending.take() else {
            return;
        };
        if pending.scope != self.cursor.scope
            || !self.adapter.attached(pending.peer)
            || self.stopping
            || self.paused
            || self.update_hold.is_some()
        {
            return; // A focus/session/safety boundary never replays old input.
        }
        // The press may have begun while this packet waited for its FIFO read.
        // Apply the same discard policy before either reconciliation or timeout.
        if self.inject.discard_source_motion() {
            return;
        }
        if self.now.saturating_duration_since(pending.at) >= MAX_WAIT {
            self.cursor.diagnostics.timeout(
                self.now.saturating_duration_since(pending.at).as_micros() as u64,
                self.micros(),
                self.inject.trace.as_deref(),
            );
            self.cursor.late = Some(pending.read);
            // No sample, rebase, clip refresh, fault or retroactive replay. Route
            // from today's predicted state and let later input follow in FIFO.
            self.input_time = pending.time;
            self.input_report = pending.report;
            self.adapter.inbound(pending.peer, pending.message);
            self.outputs(pending.source);
            if let Some((source, time, arrived, lossy)) = pending.arrival {
                self.record_jitter(pending.peer, source, time, arrived, lossy);
            }
            self.input_time = None;
            self.input_report = None;
            return;
        }
        if let Some(retry) = pending.retry {
            if self.now < retry {
                self.cursor.pending = Some(pending);
                return;
            }
            pending.read = self.inject.read_cursor(pending.scope);
            pending.retry = None;
        }
        let sample = match pending.read.try_recv() {
            Ok(sample) => Some(sample),
            Err(TryRecvError::Empty) => {
                self.cursor.pending = Some(pending);
                return;
            }
            Err(TryRecvError::Disconnected) => None,
        };
        if pending.revision != self.cursor.revision {
            // Local capture advanced while the worker read was in flight. Keep
            // the raw delta, but never apply that older sample to the new base.
            pending.revision = self.cursor.revision;
            pending.read = self.inject.read_cursor(pending.scope);
            self.cursor.pending = Some(pending);
            return;
        }
        let now = self.micros();
        let decision = sample.map_or(Decision::Unavailable, |s| {
            self.cursor.reconcile.observe(pending.scope, s.observation, now)
        });
        self.cursor.diagnostics.observe(
            decision,
            sample.map_or(0, |s| s.read_us),
            self.now.saturating_duration_since(pending.at).as_micros() as u64,
            now,
            self.inject.trace.as_deref(),
        );
        match decision {
            Decision::WaitUntil(at) => {
                pending.retry = Some(self.start + Duration::from_micros(at));
                self.cursor.pending = Some(pending);
                return;
            }
            Decision::Stale => {
                pending.read = self.inject.read_cursor(pending.scope);
                self.cursor.pending = Some(pending);
                return;
            }
            _ => {}
        }
        let position = match decision {
            Decision::Rebase(p) | Decision::Ambiguous(p) => Some(p),
            _ => None,
        };
        let clip = sample.and_then(|s| s.clip);
        self.clip_poll_at = self.now + Duration::from_millis(100);
        self.clip_available = clip.is_some();
        self.adapter.router_mut().reconcile_target_snapshot(position, clip);
        // Consume exactly once, after resolving the base and constraint. Router
        // remains the sole authority for gain, gap, resistance, drag and crossing.
        self.input_time = pending.time;
        self.input_report = pending.report;
        self.adapter.inbound(pending.peer, pending.message);
        self.outputs(pending.source);
        if let Some((source, time, arrived, lossy)) = pending.arrival {
            self.record_jitter(pending.peer, source, time, arrived, lossy);
        }
        self.input_time = None;
        self.input_report = None;
    }
}
