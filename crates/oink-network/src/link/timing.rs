//! Numeric source measurements. No tracing payload, peer, address or connection
//! identity is exposed. The optional observer must be nonblocking and allocation-free.
use std::{sync::OnceLock, time::Instant};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Result {
    Ready,
    Failed,
    Cancelled,
    Timeout,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    thread_local! {
        static CAPTURE: RefCell<Option<Vec<Measurement>>> = const { RefCell::new(None) };
    }
    /// Measurements recorded on this thread while `work` runs.
    pub(crate) fn capture(work: impl FnOnce()) -> Vec<Measurement> {
        set_observer(|m, _, _| {
            CAPTURE.with(|v| {
                if let Some(events) = v.borrow_mut().as_mut() {
                    events.push(m);
                }
            })
        });
        CAPTURE.with(|v| *v.borrow_mut() = Some(Vec::new()));
        work();
        CAPTURE.with(|v| v.borrow_mut().take().unwrap())
    }
}
