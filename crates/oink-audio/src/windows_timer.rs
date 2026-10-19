//! Balanced timer-resolution request for the lifetime of active Windows audio.
//!
//! This helps timeout-driven consumers (including Tokio's IOCP waits); audio
//! device events and QPC precision do not depend on this setting. Windows 10
//! 2004+ scopes the request to this process. Windows 11 may ignore it for an
//! occluded/inaudible window-owning process, so success is not proof of 1 ms
//! scheduling. No registry, power policy, or persistent setting is changed.

use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};

/// Every successful request has exactly one matching release. Multiple active
/// Capture/Playback guards are allowed by WinMM and balanced independently.
#[derive(Debug)]
pub(crate) struct TimerResolution {
    period_ms: u32,
}

impl TimerResolution {
    pub(crate) fn request_1ms() -> Result<Self, String> {
        const PERIOD_MS: u32 = 1;
        // SAFETY: scalar period only; successful request is paired by Drop.
        let result = unsafe { timeBeginPeriod(PERIOD_MS) };
        if result == 0 {
            Ok(Self { period_ms: PERIOD_MS })
        } else {
            Err(format!("timeBeginPeriod(1) failed: MMRESULT {result}"))
        }
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        // SAFETY: exactly one release for this guard's successful request, with
        // the same period. WinMM requests are process-scoped, not thread-owned.
        let result = unsafe { timeEndPeriod(self.period_ms) };
        if result != 0 {
            tracing::warn!(period_ms = self.period_ms, mmresult = result, "timeEndPeriod failed");
        }
    }
}
