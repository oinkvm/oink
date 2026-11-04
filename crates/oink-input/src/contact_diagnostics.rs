//! Observation-only contact availability. Callback writers use atomics only;
//! sampling/formatting happens on the ordinary diagnostic consumer. Times are
//! host uptime microseconds, not framework timestamps. No geometry or identity.
use std::sync::atomic::{AtomicU64, Ordering};

/// Stable diagnostic discriminants; never sent as input capability or used by routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CaptureFailure {
    AlreadyActive = 1,
    Framework = 2,
    DeviceList = 3,
    NoBuiltIn = 4,
    AmbiguousDevices = 5,
    InvalidDimensions = 6,
    StartRejected = 7,
    ListSymbol = 10,
    BuiltInSymbol = 11,
    DimensionsSymbol = 12,
    RegisterSymbol = 13,
    UnregisterSymbol = 14,
    StartSymbol = 15,
    StopSymbol = 16,
}

const N: usize = 18;
static VALUES: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
const KEYS: [&str; N] = [
    "capture_state",
    "capture_failure",
    "capture_started_us",
    "capture_last_valid_us",
    "capture_last_lift_us",
    "capture_callbacks",
    "capture_valid_samples",
    "capture_faults",
    "capture_recoveries",
    "capture_runtime_reason",
    "capture_devices",
    "capture_built_in",
    "capture_non_builtin_other",
    "capture_enumerated",
    "capture_registered",
    "capture_start_result",
    "capture_dimensions_result",
    "capture_attempts",
];
/// Independent atomic observations, not a transaction. Zero times mean unobserved.
/// state: 0=not attempted, 1=starting, 2=subscribed, 3=unavailable, 4=stopped.
pub fn snapshot() -> impl Iterator<Item = (&'static str, u64)> {
    #[cfg(target_os = "macos")]
    let now = crate::macos::contact_snapshot_us();
    #[cfg(not(target_os = "macos"))]
    let now = 0;
    KEYS.into_iter()
        .zip(VALUES.iter().map(|v| v.load(Ordering::Relaxed)))
        .chain(std::iter::once(("capture_snapshot_us", now)))
}
#[cfg(target_os = "macos")]
pub(crate) fn begin(at: u64) {
    for value in &VALUES[..17] {
        value.store(0, Ordering::Relaxed);
    }
    VALUES[17].fetch_add(1, Ordering::Relaxed);
    set(0, 1);
    set(2, at);
}
#[cfg(target_os = "macos")]
pub(crate) fn set(index: usize, value: u64) {
    VALUES[index].store(value, Ordering::Relaxed);
}
#[cfg(target_os = "macos")]
pub(crate) fn failure(reason: CaptureFailure) {
    set(1, reason as u64);
    set(0, 3);
    crate::native_lifecycle::emit_lifecycle(
        crate::native_lifecycle::Kind::ContactCapture,
        crate::native_lifecycle::Data {
            message: 5,
            reason: reason as u32,
            success: Some(false),
            ..Default::default()
        },
    );
}
#[cfg(target_os = "macos")]
pub(crate) fn sample(at: u64, valid: bool, lift: bool) {
    VALUES[5].fetch_add(1, Ordering::Relaxed);
    if valid {
        set(3, at);
        VALUES[6].fetch_add(1, Ordering::Relaxed);
        if lift {
            set(4, at);
            if VALUES[9].swap(0, Ordering::Relaxed) != 0 {
                VALUES[8].fetch_add(1, Ordering::Relaxed);
                crate::native_lifecycle::emit_lifecycle(
                    crate::native_lifecycle::Kind::ContactCapture,
                    crate::native_lifecycle::Data {
                        message: 4,
                        success: Some(true),
                        ..Default::default()
                    },
                );
            }
        }
    }
}
#[cfg(target_os = "macos")]
pub(crate) fn fault(reason: u8) {
    VALUES[7].fetch_add(1, Ordering::Relaxed);
    set(9, reason.into());
}
