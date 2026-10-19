//! Read-only snapshot shared between threads: one thread builds a whole immutable value, others read it; an update replaces
//! it whole. Readers take no lock, so realtime threads may read snapshots produced by
//! ordinary threads and the other way round.
use arc_swap::ArcSwap;
use std::sync::Arc;

pub struct Snapshot<T>(ArcSwap<T>);

impl<T> Snapshot<T> {
    pub fn new(value: T) -> Self {
        Self(ArcSwap::from_pointee(value))
    }
    /// Replace the whole value. Called by the one producer.
    pub fn publish(&self, value: T) {
        self.0.store(Arc::new(value));
    }
    /// The current value. Never blocks.
    pub fn load(&self) -> Arc<T> {
        self.0.load_full()
    }
}

impl<T: Default> Default for Snapshot<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> std::fmt::Debug for Snapshot<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Snapshot(..)")
    }
}
