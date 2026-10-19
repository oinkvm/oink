//! Interval metadata only. Native capture and injection owners use scoped TLS,
//! avoiding a lock, allocation, IPC, or a change to native event posting times.
use oink_foundation::DeviceId;
pub use oink_foundation::input::CaptureTime;
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InjectionSource {
    pub origin: DeviceId,
    pub time: CaptureTime,
}
thread_local! {
    static CAPTURE: Cell<Option<CaptureTime>> = const { Cell::new(None) };
    static INJECTION: Cell<Option<InjectionSource>> = const { Cell::new(None) };
}
pub fn injection_source() -> Option<InjectionSource> {
    INJECTION.get()
}
pub fn capture_time() -> CaptureTime {
    CAPTURE.get().unwrap_or_else(now)
}
/// Fallback for recovery/synthetic events, in the same clock as native capture.
fn now() -> CaptureTime {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn clock_gettime_nsec_np(clock_id: libc::clockid_t) -> u64;
        }
        // CGEventGetTimestamp uses uptime raw; Instant's private epoch differs.
        // SAFETY: read-only monotonic system clock, no handles or pointers.
        CaptureTime::from_micros(unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) } / 1000)
    }
    #[cfg(windows)]
    {
        use std::sync::OnceLock;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn QueryPerformanceCounter(value: *mut i64) -> i32;
            fn QueryPerformanceFrequency(value: *mut i64) -> i32;
        }
        static FREQUENCY: OnceLock<u64> = OnceLock::new();
        let frequency = *FREQUENCY.get_or_init(|| {
            let mut value = 0;
            // SAFETY: valid local output pointer; QPC is always available on our supported Windows.
            assert_ne!(unsafe { QueryPerformanceFrequency(&mut value) }, 0);
            value as u64
        });
        let mut ticks = 0;
        // LL hook/GetMessageTime is quantised by the system timer (often
        // 10–16ms), obscuring a LAN's few-ms variation. Measure QPC at entry;
        // OS queue wait remains its separate coarse metric. Absolute QPC also
        // preserves the capture clock across process restarts and relays.
        // SAFETY: valid local output pointer, read-only monotonic clock.
        assert_ne!(unsafe { QueryPerformanceCounter(&mut ticks) }, 0);
        let ticks = ticks as u64;
        CaptureTime::from_micros(ticks / frequency * 1_000_000 + ticks % frequency * 1_000_000 / frequency)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        CaptureTime::from_micros(START.get_or_init(Instant::now).elapsed().as_micros() as u64)
    }
}
#[derive(Debug)]
pub struct CaptureScope(Option<CaptureTime>);
impl CaptureScope {
    pub fn new(time: CaptureTime) -> Self {
        Self(CAPTURE.replace(Some(time)))
    }
    pub fn entry() -> Self {
        Self::new(now())
    }
}
impl Drop for CaptureScope {
    fn drop(&mut self) {
        CAPTURE.set(self.0);
    }
}
pub fn with_injection_source<T>(source: Option<InjectionSource>, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<InjectionSource>);
    impl Drop for Restore {
        fn drop(&mut self) {
            INJECTION.set(self.0);
        }
    }
    let _restore = Restore(INJECTION.replace(source));
    f()
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn intervals_survive_wrap_loss_and_reordering() {
        let old = CaptureTime(u32::MAX - 99);
        let next = CaptureTime(200);
        assert_eq!(next.elapsed_since(old), Some(300));
        assert_eq!(CaptureTime(900).elapsed_since(old), Some(1000));
        assert_eq!(old.elapsed_since(next), None);
        assert_eq!(CaptureTime(1 << 31).elapsed_since(CaptureTime(0)), None);
    }
    #[test]
    fn injection_context_is_scoped_and_restored_after_unwind() {
        let source = InjectionSource {
            origin: DeviceId([3; 32]),
            time: CaptureTime(42),
        };
        with_injection_source(Some(source), || {
            assert_eq!(injection_source(), Some(source));
            let result = std::panic::catch_unwind(|| {
                with_injection_source(None, || {
                    assert_eq!(injection_source(), None);
                    panic!("native failure");
                })
            });
            assert!(result.is_err());
            assert_eq!(injection_source(), Some(source));
        });
        assert_eq!(injection_source(), None);
    }
}
