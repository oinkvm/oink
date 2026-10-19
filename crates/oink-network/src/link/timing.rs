//! Numeric source measurements. No tracing payload, peer, address or connection
//! identity is exposed. The optional observer must be nonblocking and allocation-free.
use std::{sync::OnceLock, time::Instant};
#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Dial,
    InstanceAuth,
    Selection,
    PathInstall,
    ConfigReady,
    Migration,
    LowTcp,
    LowTls,
    LowBind,
    LowReady,
}
#[derive(Clone, Copy, Debug)]
pub enum Result {
    Ready,
    Failed,
    Cancelled,
    Timeout,
}
#[derive(Clone, Copy, Debug)]
pub enum Measurement {
    Setup(Result),
    Stage(Stage),
}
static OBSERVER: OnceLock<fn(Measurement, Instant, Instant)> = OnceLock::new();
pub fn set_observer(observer: fn(Measurement, Instant, Instant)) {
    let _ = OBSERVER.set(observer);
}
pub(crate) fn record(measurement: Measurement, start: Instant, end: Instant) {
    if let Some(observer) = OBSERVER.get() {
        observer(measurement, start, end);
    }
}
