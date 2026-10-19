//! Native visual health, without screen/device names, window IDs or pixels.
//! Counters observe work; they never repair, reorder or wait for native work.
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

#[cfg(target_os = "macos")]
pub(crate) mod macos;

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub(crate) enum Kind {
    Trail = 1,
    Projection = 2,
}

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub(crate) enum Event {
    Created = 1,
    WorkerStarted,
    WorkerStopped,
    WorkerFailed,
    Panic,
    Intent,
    Displays,
    Hide,
    Summary,
    #[cfg(target_os = "macos")]
    Visibility,
    #[cfg(target_os = "macos")]
    EmptyWake,
    #[cfg(target_os = "macos")]
    StaleGeneration,
    #[cfg(target_os = "macos")]
    Environment,
}

/// Hide reasons are about the renderer, not a guess about WindowServer state.
#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum Hide {
    Explicit = 1,
    LeaseExpired,
    Paused,
    Idle,
    Shutdown,
    NoBounds,
}

fn registry() -> &'static Mutex<Vec<Arc<Health>>> {
    static REGISTRY: OnceLock<Mutex<Vec<Arc<Health>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

#[derive(Debug)]
pub(crate) struct Health {
    pub(crate) id: u64,
    pub(crate) kind: Kind,
    origin: Instant,
    worker: AtomicU64,
    flags: AtomicU64,
    desired: AtomicU64,
    frames: AtomicU64,
    applied: AtomicU64,
    posted: AtomicU64,
    started: AtomicU64,
    completed: AtomicU64,
    dropped: AtomicU64,
    pending: AtomicU64,
    busy_since: AtomicU64,
    last_apply: AtomicU64,
    panics: AtomicU64,
    last_report: AtomicU64,
    #[cfg(target_os = "macos")]
    pub(crate) sampling: AtomicBool,
}

impl Health {
    pub(crate) fn new(kind: Kind) -> Arc<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let h = Arc::new(Self {
            id: NEXT.fetch_add(1, Relaxed),
            kind,
            origin: Instant::now(),
            worker: AtomicU64::new(0),
            flags: AtomicU64::new(0),
            desired: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            applied: AtomicU64::new(0),
            posted: AtomicU64::new(0),
            started: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            pending: AtomicU64::new(0),
            busy_since: AtomicU64::new(0),
            last_apply: AtomicU64::new(0),
            panics: AtomicU64::new(0),
            last_report: AtomicU64::new(0),
            #[cfg(target_os = "macos")]
            sampling: AtomicBool::new(false),
        });
        let mut r = crate::lock(registry());
        // Startup precedes Recorder registration. Keep bounded numeric tombstones
        // so even a failed spawn can be inspected by the first later export.
        if r.len() == 64 {
            r.remove(0);
        }
        r.push(h.clone());
        drop(r);
        h.event(Event::Created, 0);
        h
    }
    fn now(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64 + 1
    }
    pub(crate) fn event(&self, event: Event, reason: u64) {
        tracing::info!(target: "oink_overlay_diagnostics", overlay_object=self.id,
            overlay_kind=self.kind as u8, overlay_event=event as u8, overlay_reason=reason);
    }
    pub(crate) fn worker(self: &Arc<Self>) -> Worker {
        self.worker.store(1, Relaxed);
        self.event(Event::WorkerStarted, 0);
        Worker(self.clone())
    }
    pub(crate) fn failed(&self) {
        self.worker.store(3, Relaxed);
        self.event(Event::WorkerFailed, 0);
    }
    pub(crate) fn panic(&self) {
        self.panics.fetch_add(1, Relaxed);
        self.event(Event::Panic, 0);
    }
    pub(crate) fn intent(&self, flags: u64) {
        if self.flags.swap(flags, Relaxed) != flags {
            self.event(Event::Intent, flags);
        }
    }
    pub(crate) fn frame(&self, count: usize) {
        self.desired.store(count as u64, Relaxed);
        self.frames.fetch_add(1, Relaxed);
    }
    pub(crate) fn applied(&self) {
        self.applied.fetch_add(1, Relaxed);
        self.last_apply.store(self.now(), Relaxed);
    }
    pub(crate) fn hide(&self, reason: Hide) {
        self.desired.store(0, Relaxed);
        self.event(Event::Hide, reason as u64);
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn dropped(&self) {
        self.dropped.fetch_add(1, Relaxed);
    }
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn ticket(self: &Arc<Self>) -> Ticket {
        self.posted.fetch_add(1, Relaxed);
        if self.pending.fetch_add(1, Relaxed) == 0 {
            self.busy_since.store(self.now(), Relaxed);
        }
        Ticket(self.clone())
    }
    pub(crate) fn pulse(self: &Arc<Self>) {
        let now = self.now();
        let last = self.last_report.load(Relaxed);
        if (last == 0 || now.saturating_sub(last) >= 1000)
            && self.last_report.compare_exchange(last, now, Relaxed, Relaxed).is_ok()
        {
            self.report();
            self.sample();
        }
    }
    fn sample(self: &Arc<Self>) {
        #[cfg(target_os = "macos")]
        macos::sample(self.clone());
    }
    fn report(&self) {
        let pending = self.pending.load(Relaxed);
        let now = self.now();
        let applied = self.last_apply.load(Relaxed);
        tracing::info!(target: "oink_overlay_diagnostics", overlay_object=self.id,
            overlay_kind=self.kind as u8, overlay_event=Event::Summary as u8,
            overlay_worker=self.worker.load(Relaxed), overlay_flags=self.flags.load(Relaxed),
            overlay_desired=self.desired.load(Relaxed), overlay_frames=self.frames.load(Relaxed),
            overlay_applied=self.applied.load(Relaxed), overlay_posted=self.posted.load(Relaxed),
            overlay_started=self.started.load(Relaxed), overlay_completed=self.completed.load(Relaxed),
            overlay_dropped=self.dropped.load(Relaxed), overlay_pending=pending,
            // Continuous nonempty-queue age, not the age of its oldest job.
            overlay_busy_ms=if pending == 0 { 0 } else { now.saturating_sub(self.busy_since.load(Relaxed)) },
            overlay_apply_known=applied != 0, overlay_apply_age_ms=if applied == 0 { 0 } else { now.saturating_sub(applied) },
            overlay_panics=self.panics.load(Relaxed));
    }
}

pub(crate) struct Worker(Arc<Health>);
impl Drop for Worker {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.worker.store(4, Relaxed);
            self.0.panic();
        } else if self.0.worker.load(Relaxed) != 3 {
            self.0.worker.store(2, Relaxed);
            self.0.event(Event::WorkerStopped, 0);
        }
    }
}

#[cfg(any(target_os = "macos", test))]
pub(crate) struct Ticket(Arc<Health>);
#[cfg(any(target_os = "macos", test))]
impl Ticket {
    pub(crate) fn start(&self) {
        self.0.started.fetch_add(1, Relaxed);
    }
}
#[cfg(any(target_os = "macos", test))]
impl Drop for Ticket {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.panic();
        }
        self.0.completed.fetch_add(1, Relaxed);
        self.0.pending.fetch_sub(1, Relaxed);
    }
}

/// Export also reports idle/failed workers and queues whose main-thread sample
/// cannot run. It never waits for AppKit, so a deadlock cannot block collection.
pub fn report_all() {
    let live = crate::lock(registry()).clone();
    for h in live {
        h.report();
        h.sample();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stalled_and_panicking_jobs_remain_distinguishable() {
        let h = Health::new(Kind::Trail);
        let first = h.ticket();
        assert_eq!(h.posted.load(Relaxed), 1);
        assert_eq!(h.started.load(Relaxed), 0);
        assert_eq!(h.pending.load(Relaxed), 1);
        first.start();
        let result = std::panic::catch_unwind(|| {
            let _job = first;
            panic!("test");
        });
        assert!(result.is_err());
        assert_eq!(h.completed.load(Relaxed), 1);
        assert_eq!(h.pending.load(Relaxed), 0);
        let _ = std::panic::catch_unwind(|| {
            let _worker = h.worker();
            panic!("test");
        });
        assert_eq!(h.worker.load(Relaxed), 4);
        assert_eq!(h.panics.load(Relaxed), 2);
    }
    #[test]
    fn failure_is_not_reported_as_clean_worker_stop() {
        let h = Health::new(Kind::Projection);
        {
            let _worker = h.worker();
            h.failed();
        }
        assert_eq!(h.worker.load(Relaxed), 3);
    }
}
