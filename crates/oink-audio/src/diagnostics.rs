//! Content-free timing summaries. Buckets are <=1, 2, 5, 10, 20, 50, 100 ms,
//! and >100 ms; a snapshot resets only the reporting window, never audio state.
use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone, Copy)]
pub struct Histogram {
    pub buckets: [u64; 8],
    pub max_us: u64,
}
impl Histogram {
    pub fn observe(&mut self, elapsed: Duration) {
        let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        let index = [1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000].partition_point(|bound| micros > *bound);
        self.buckets[index] += 1;
        self.max_us = self.max_us.max(micros);
    }
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

#[derive(Debug, Default)]
pub struct Cadence {
    previous: Option<Instant>,
    pub gaps: Histogram,
}
impl Cadence {
    pub fn observe(&mut self, at: Instant) {
        if let Some(previous) = self.previous {
            self.gaps.observe(at.saturating_duration_since(previous));
        }
        self.previous = Some(at);
    }
}

/// Process-local diagnostic epoch. Values may be negative for an older native
/// callback collected after initialization. Never compare these across hosts.
pub fn monotonic_us(at: Instant) -> i64 {
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let origin = *ORIGIN.get_or_init(Instant::now);
    if at >= origin {
        at.duration_since(origin).as_micros().min(i64::MAX as u128) as i64
    } else {
        -(origin.duration_since(at).as_micros().min(i64::MAX as u128) as i64)
    }
}

/// Fixed storage; observing events cannot grow memory, wait, or affect audio.
#[derive(Debug, Clone, Copy)]
pub struct History<T: Copy, const N: usize> {
    pub events: [Option<T>; N],
    pub next: usize,
}
impl<T: Copy, const N: usize> Default for History<T, N> {
    fn default() -> Self {
        Self {
            events: [None; N],
            next: 0,
        }
    }
}
impl<T: Copy, const N: usize> History<T, N> {
    pub fn push(&mut self, event: T) {
        self.events[self.next] = Some(event);
        self.next = (self.next + 1) % N;
    }
}

/// Read-only inspection of the calling thread; never changes scheduling policy.
pub fn log_thread_scheduling(role: &str) {
    #[cfg(target_os = "macos")]
    crate::macos::log_thread_scheduling(std::ptr::null_mut(), role);
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, GetCurrentThread, GetCurrentThreadId, GetPriorityClass, GetThreadPriority,
        };
        // SAFETY: pseudo-handles refer to this live process/thread; read-only queries.
        let (thread_id, process_class, relative_priority) = unsafe {
            (
                GetCurrentThreadId(),
                GetPriorityClass(GetCurrentProcess()),
                GetThreadPriority(GetCurrentThread()),
            )
        };
        tracing::info!(
            role,
            thread_id,
            process_class,
            relative_priority,
            "audio thread scheduling (read only)"
        );
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    tracing::info!(role, "audio scheduling inspection unavailable on this platform");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_history_overwrites_diagnostics_only_and_epoch_handles_old_events() {
        let mut history = History::<u64, 4>::default();
        for value in 0..100 {
            history.push(value);
        }
        assert_eq!(history.events, [Some(96), Some(97), Some(98), Some(99)]);
        assert_eq!(history.next, 0);
        let now = Instant::now();
        // Signed microsecond truncation can differ by one across the epoch.
        let interval = monotonic_us(now) - monotonic_us(now - Duration::from_millis(30));
        assert!(interval.abs_diff(30_000) <= 1);
    }

    #[test]
    fn histogram_boundaries_and_window_reset() {
        let mut histogram = Histogram::default();
        for us in [0, 1000, 1001, 2000, 5000, 10000, 20000, 50000, 100000, 100001] {
            histogram.observe(Duration::from_micros(us));
        }
        let snapshot = histogram.take();
        assert_eq!(snapshot.buckets, [2, 2, 1, 1, 1, 1, 1, 1]);
        assert_eq!(snapshot.max_us, 100001);
        assert_eq!(histogram.buckets, [0; 8]);
    }
}
