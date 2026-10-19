//! Device-cadenced auxiliary audio workers. Native callbacks only signal the
//! coalescing wakeup; heap/OS object creation and destruction stay outside work.
use std::{
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug)]
pub struct Signal(Arc<Wake>);
#[derive(Debug)]
struct Wake {
    pending: AtomicBool,
    #[cfg(target_os = "macos")]
    handle: u32,
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
}
// SAFETY: OS semaphore/event operations are thread-safe; Wake owns its handle,
// which remains live until all Signal owners (including the callback) are gone.
unsafe impl Send for Wake {}
// SAFETY: all shared fields are atomic or a stable thread-safe OS handle.
unsafe impl Sync for Wake {}
impl Signal {
    pub fn new() -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        let handle = {
            let mut error = 0;
            // SAFETY: writable scalar; returns one owned Mach semaphore.
            let handle = unsafe { oink_audio_wake_create(&mut error) };
            if error != 0 {
                return Err(format!("create audio wake semaphore: {error}"));
            }
            handle
        };
        #[cfg(windows)]
        // SAFETY: unnamed auto-reset event, owned until Drop.
        let handle = unsafe { windows::Win32::System::Threading::CreateEventW(None, false, false, None) }
            .map_err(|error| format!("create audio wake event: {error}"))?;
        Ok(Self(Arc::new(Wake {
            pending: AtomicBool::new(false),
            #[cfg(any(windows, target_os = "macos"))]
            handle,
        })))
    }
    /// Real-time safe, coalesced to one outstanding work notification. Does not
    /// wait for the worker or release any shared ownership.
    pub fn notify(&self) {
        if self.0.pending.swap(true, Ordering::Release) {
            return;
        }
        #[cfg(target_os = "macos")]
        // SAFETY: Arc keeps the OS semaphore live for this call.
        unsafe {
            oink_audio_wake_signal(self.0.handle);
        }
        #[cfg(windows)]
        // SAFETY: Arc keeps this event live; SetEvent does not wait for a peer.
        unsafe {
            let _ = wake_set_event(self.0.handle.0);
        }
    }
    /// Scheduling boundary only; never called in a work unit. There is one
    /// waiter per Signal. A stop request must notify it before joining.
    pub fn wait(&self) -> Result<(), i32> {
        #[cfg(target_os = "macos")]
        // SAFETY: sole waiter; handle is live. Errors retire the worker through its owner.
        let result = unsafe { oink_audio_wake_wait(self.0.handle) };
        #[cfg(windows)]
        // SAFETY: sole waiter; handle remains live through this call.
        let result =
            unsafe { windows::Win32::System::Threading::WaitForSingleObject(self.0.handle, u32::MAX) }.0 as i32;
        #[cfg(not(any(target_os = "macos", windows)))]
        let result = {
            std::thread::sleep(std::time::Duration::from_millis(1));
            0
        };
        self.0.pending.store(false, Ordering::Release);
        if result == 0 { Ok(()) } else { Err(result) }
    }
}
impl Drop for Wake {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        // SAFETY: last owner; native callbacks and waiter have been joined.
        unsafe {
            oink_audio_wake_destroy(self.handle);
        }
        #[cfg(windows)]
        // SAFETY: last owner; no OS operation can still reference this handle.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// Prepared on an ordinary thread, moved once to a newly spawned worker. The
/// workgroup reference is tied to the actual stream's device, never a default
/// device lookup that could race a route change.
#[derive(Debug)]
pub struct Schedule {
    #[cfg(target_os = "macos")]
    raw: *mut std::ffi::c_void,
}
// SAFETY: preparation retains a workgroup but does not join it. Schedule is
// moved to one thread; enter needs &mut and its guard is neither Send nor Sync.
unsafe impl Send for Schedule {}
impl Schedule {
    #[cfg(target_os = "macos")]
    pub(crate) fn device(device: u32, period_ns: u64) -> Result<Self, String> {
        let mut error = 0;
        // SAFETY: valid scalar outputs; C retains the workgroup for this device.
        let raw = unsafe { oink_realtime_create(device, period_ns, &mut error) };
        if raw.is_null() {
            Err(format!("prepare audio device workgroup: {error}"))
        } else {
            Ok(Self { raw })
        }
    }
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn device(_: u32, _: u64) -> Result<Self, String> {
        Ok(Self {})
    }

    /// Startup boundary, before the first work unit. Return a numeric error so
    /// any description/logging can happen after leaving the scheduling class.
    pub fn enter(&mut self) -> Result<Guard<'_>, i32> {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: exclusive Schedule on this fresh auxiliary thread.
            let error = unsafe { oink_realtime_enter(self.raw) };
            if error != 0 {
                return Err(error);
            }
        }
        #[cfg(windows)]
        let handle = {
            use windows::{Win32::System::Threading::AvSetMmThreadCharacteristicsW, core::w};
            let mut index = 0;
            // SAFETY: thread-affine guard balances successful registration.
            unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) }.map_err(|e| e.code().0)?
        };
        Ok(Guard {
            schedule: self,
            _affine: PhantomData,
            #[cfg(windows)]
            handle,
        })
    }
}
impl Drop for Schedule {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        // SAFETY: borrow guarantees Guard has left before Schedule is released.
        unsafe {
            oink_realtime_destroy(self.raw);
        }
    }
}
pub struct Guard<'a> {
    schedule: &'a mut Schedule,
    _affine: PhantomData<Rc<()>>,
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
}
impl std::fmt::Debug for Guard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealtimeGuard").finish_non_exhaustive()
    }
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        // SAFETY: thread-affine guard; leaves workgroup before reverting policy.
        unsafe {
            oink_realtime_leave(self.schedule.raw);
        }
        #[cfg(windows)]
        // SAFETY: this guard cannot leave the thread that registered MMCSS.
        unsafe {
            let _ = windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.handle);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = &self.schedule;
    }
}
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "SetEvent"]
    fn wake_set_event(handle: *mut std::ffi::c_void) -> i32;
}
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn oink_audio_wake_create(error: *mut i32) -> u32;
    fn oink_audio_wake_signal(handle: u32) -> i32;
    fn oink_audio_wake_wait(handle: u32) -> i32;
    fn oink_audio_wake_destroy(handle: u32);
    fn oink_realtime_create(device: u32, period_ns: u64, error: *mut i32) -> *mut std::ffi::c_void;
    fn oink_realtime_enter(raw: *mut std::ffi::c_void) -> i32;
    fn oink_realtime_leave(raw: *mut std::ffi::c_void) -> i32;
    fn oink_realtime_destroy(raw: *mut std::ffi::c_void);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Metrics {
    pub cycles: u64,
    pub gaps: crate::diagnostics::Histogram,
    pub max_work_us: u64,
}
#[derive(Debug, Default)]
pub(crate) struct Counters {
    cycles: std::sync::atomic::AtomicU64,
    buckets: [std::sync::atomic::AtomicU64; 8],
    max_gap_us: std::sync::atomic::AtomicU64,
    max_work_us: std::sync::atomic::AtomicU64,
}
impl Counters {
    pub(crate) fn observe(&self, gap: Option<std::time::Duration>, work: std::time::Duration) {
        self.cycles.fetch_add(1, Ordering::Relaxed);
        if let Some(gap) = gap {
            let us = gap.as_micros().min(u128::from(u64::MAX)) as u64;
            let index = [1000, 2000, 5000, 10000, 20000, 50000, 100000].partition_point(|bound| us > *bound);
            self.buckets[index].fetch_add(1, Ordering::Relaxed);
            self.max_gap_us.fetch_max(us, Ordering::Relaxed);
        }
        self.max_work_us
            .fetch_max(work.as_micros().min(u128::from(u64::MAX)) as u64, Ordering::Relaxed);
    }
    pub(crate) fn take(&self) -> Metrics {
        Metrics {
            cycles: self.cycles.swap(0, Ordering::Relaxed),
            gaps: crate::diagnostics::Histogram {
                buckets: std::array::from_fn(|i| self.buckets[i].swap(0, Ordering::Relaxed)),
                max_us: self.max_gap_us.swap(0, Ordering::Relaxed),
            },
            max_work_us: self.max_work_us.swap(0, Ordering::Relaxed),
        }
    }
}
