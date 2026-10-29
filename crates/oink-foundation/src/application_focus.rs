//! Local platform observations. Native app/window identities stay in the backend.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObservationStamp {
    /// Changes when the observer/session is recreated. Never reused within a Boot.
    pub generation: u64,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Foreground {
    Hidden,
    Windowed,
    Fullscreen,
    /// An ordinary app is known, but fullscreen classification failed. Do not park.
    OrdinaryUnknown,
    #[default]
    Unknown,
    Unavailable,
}
impl Foreground {
    pub fn ordinary(self) -> bool {
        matches!(self, Self::Windowed | Self::Fullscreen | Self::OrdinaryUnknown)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub stamp: ObservationStamp,
    pub foreground: Foreground,
    /// Latched by the observer until an ordinary foreground can be identified.
    pub hidden_lost: bool,
    /// Only a snapshot produced by this park operation may confirm its own change.
    pub park_operation: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkResult {
    Confirmed,
    FullscreenExempt,
    UnknownExempt,
    Denied,
    Stale,
    Unavailable,
}

/// Explicit time for fake-backend integration fixtures. No wall-clock deadline
/// elapses merely because a loaded CI host deschedules an owner. Production
/// backends never provide this clock; release builds do not contain it.
#[cfg(debug_assertions)]
#[derive(Clone, Debug, Default)]
pub struct TestClock(std::sync::Arc<std::sync::atomic::AtomicU64>);
#[cfg(debug_assertions)]
impl TestClock {
    pub fn now(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
    /// Advance microseconds, then wake the routing owner to process its timers.
    pub fn advance(&self, micros: u64) {
        self.0.fetch_add(micros, std::sync::atomic::Ordering::AcqRel);
    }
}
