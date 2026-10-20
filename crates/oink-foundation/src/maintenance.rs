//! Process-local activity admission for a coordinated update. This contains no
//! updater, IPC or wire policy; resource owners hold a lease through completion.
use std::{
    collections::BTreeMap,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActivityKind {
    Drag,
    Promise,
    FileProvider,
    FileTransfer,
    Audio,
    Configuration,
}
#[derive(Debug, Default)]
struct State {
    closed: Option<u64>,
    active: BTreeMap<ActivityKind, usize>,
}
#[derive(Debug)]
struct Inner {
    state: Mutex<State>,
    input_generation: AtomicU64,
    last_input_ms: AtomicU64,
    start: Instant,
}
#[derive(Clone, Debug)]
pub struct Gate(Arc<Inner>);
#[derive(Debug)]
pub struct Lease {
    gate: Gate,
    kind: ActivityKind,
}
impl Default for Gate {
    fn default() -> Self {
        Self(Arc::new(Inner {
            state: Mutex::default(),
            input_generation: AtomicU64::new(0),
            last_input_ms: AtomicU64::new(0),
            start: Instant::now(),
        }))
    }
}
impl Gate {
    pub fn enter(&self, kind: ActivityKind) -> Option<Lease> {
        let mut s = self.0.state.lock().unwrap();
        if s.closed.is_some() {
            return None;
        }
        *s.active.entry(kind).or_default() += 1;
        Some(Lease {
            gate: self.clone(),
            kind,
        })
    }
    /// Close admission before taking the busy snapshot. Existing owners continue;
    /// the coordinator reopens on Busy, timeout, cancellation or changed input.
    pub fn close(&self, generation: u64) -> Result<Vec<ActivityKind>, u64> {
        let mut s = self.0.state.lock().unwrap();
        if let Some(other) = s.closed
            && other != generation
        {
            return Err(other);
        }
        if s.active.is_empty() {
            s.closed = Some(generation);
        }
        Ok(s.active.keys().copied().collect())
    }
    /// An explicit Install interrupts existing owners instead of waiting for
    /// silence/idle. Close admission atomically; owners then tear down before
    /// replacement, and cancellation reopens the same generation.
    pub fn close_for_update(&self, generation: u64) -> Result<(), u64> {
        let mut state = self.0.state.lock().unwrap();
        if let Some(other) = state.closed
            && other != generation
        {
            return Err(other);
        }
        state.closed = Some(generation);
        Ok(())
    }
    pub fn reopen(&self, generation: u64) {
        let mut s = self.0.state.lock().unwrap();
        if s.closed == Some(generation) {
            s.closed = None;
        }
    }
    pub fn generation(&self) -> Option<u64> {
        self.0.state.lock().unwrap().closed
    }
    pub fn closed(&self) -> bool {
        self.0.state.lock().unwrap().closed.is_some()
    }
    pub fn busy(&self) -> Vec<ActivityKind> {
        self.0.state.lock().unwrap().active.keys().copied().collect()
    }
    /// Safe in input callbacks: only atomics, no queue, allocation, or mutex.
    pub fn user_input(&self) {
        self.0
            .last_input_ms
            .store(self.0.start.elapsed().as_millis() as u64, Ordering::Release);
        self.0.input_generation.fetch_add(1, Ordering::AcqRel);
    }
    pub fn input_generation(&self) -> u64 {
        self.0.input_generation.load(Ordering::Acquire)
    }
    pub fn idle_millis(&self) -> u64 {
        (self.0.start.elapsed().as_millis() as u64).saturating_sub(self.0.last_input_ms.load(Ordering::Acquire))
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut s = self.gate.0.state.lock().unwrap();
        if let Some(n) = s.active.get_mut(&self.kind) {
            *n -= 1;
            if *n == 0 {
                s.active.remove(&self.kind);
            }
        }
    }
}
/// Each installed backend owns one process. Content callbacks in platform modules
/// use this same gate, including callbacks after the drag has ended.
pub fn process() -> &'static Gate {
    static GATE: LazyLock<Gate> = LazyLock::new(Gate::default);
    &GATE
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn close_and_enter_are_atomic_and_existing_work_survives() {
        let g = Gate::default();
        let lease = g.enter(ActivityKind::Promise).unwrap();
        assert_eq!(g.close(1).unwrap(), vec![ActivityKind::Promise]);
        assert!(!g.closed());
        drop(lease);
        assert!(g.close(1).unwrap().is_empty());
        assert!(g.enter(ActivityKind::Audio).is_none());
        g.reopen(2);
        assert!(g.closed());
        g.reopen(1);
        assert!(g.enter(ActivityKind::Audio).is_some());
    }
    #[test]
    fn manual_install_closes_admission_with_every_owner_active() {
        let gate = Gate::default();
        let leases: Vec<_> = [
            ActivityKind::Drag,
            ActivityKind::Promise,
            ActivityKind::Audio,
            ActivityKind::FileProvider,
            ActivityKind::FileTransfer,
            ActivityKind::Configuration,
        ]
        .into_iter()
        .map(|kind| gate.enter(kind).unwrap())
        .collect();
        gate.close_for_update(7).unwrap();
        assert_eq!(gate.generation(), Some(7));
        assert!(gate.enter(ActivityKind::Audio).is_none());
        assert_eq!(gate.close_for_update(8), Err(7));
        gate.reopen(7);
        assert!(gate.enter(ActivityKind::Promise).is_some());
        drop(leases);
    }
    #[test]
    fn input_changes_invalidate_preparation_generation() {
        let g = Gate::default();
        let generation = g.input_generation();
        g.user_input();
        assert_ne!(generation, g.input_generation());
    }
}
