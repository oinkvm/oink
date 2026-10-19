//! Scheduling for application-owned audio delivery / codec workers.
//!
//! Hardware callbacks have their own CoreAudio / WASAPI scheduling. Protecting
//! only those callbacks cannot prevent starvation of the thread supplying them.
use std::{marker::PhantomData, rc::Rc};

/// A thread-affine, balanced scheduling request. Never changes process priority,
/// endpoint settings, buffer sizes, or persistent system policy.
#[derive(Debug)]
pub struct AudioWorker {
    _thread_affine: PhantomData<Rc<()>>,
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
    #[cfg(target_os = "macos")]
    previous: (u32, i32),
}

impl AudioWorker {
    pub fn enter(role: &'static str) -> Result<Self, String> {
        #[cfg(windows)]
        {
            use windows::{Win32::System::Threading::AvSetMmThreadCharacteristicsW, core::w};
            let mut index = 0;
            // SAFETY: current thread only; the !Send guard reverts on that thread.
            let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) }
                .map_err(|error| format!("{role}: MMCSS Pro Audio registration failed: {error}"))?;
            tracing::info!(role, "audio worker registered with MMCSS Pro Audio");
            crate::diagnostics::log_thread_scheduling(role);
            Ok(Self {
                handle,
                _thread_affine: PhantomData,
            })
        }
        #[cfg(target_os = "macos")]
        {
            let mut qos = 0;
            let mut relative = 0;
            // SAFETY: pthread_self identifies this live thread; output pointers
            // are writable, and QoS is restored on this same thread by Drop.
            let result = unsafe { pthread_get_qos_class_np(pthread_self(), &mut qos, &mut relative) };
            if result != 0 {
                return Err(format!("{role}: read audio worker QoS failed: {result}"));
            }
            // The worker also handles control messages and is not an I/O-synchronous
            // real-time callback. Use explicit USER_INTERACTIVE QoS rather than a
            // Mach time-constraint policy or claiming membership in an audio workgroup.
            // SAFETY: documented QoS constant and priority 0, current thread only.
            let result = unsafe { pthread_set_qos_class_self_np(0x21, 0) };
            if result != 0 {
                return Err(format!("{role}: set audio worker QoS failed: {result}"));
            }
            crate::diagnostics::log_thread_scheduling(role);
            Ok(Self {
                previous: (qos, relative),
                _thread_affine: PhantomData,
            })
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = role;
            Ok(Self {
                _thread_affine: PhantomData,
            })
        }
    }
}

impl Drop for AudioWorker {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: guard cannot move to another thread and owns this registration.
        if let Err(error) = unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.handle) } {
            tracing::warn!(%error, "could not revert audio worker MMCSS registration");
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: restore the current thread's previously queried QoS.
            let result = unsafe { pthread_set_qos_class_self_np(self.previous.0, self.previous.1) };
            if result != 0 {
                tracing::warn!(result, "could not restore audio worker QoS");
            }
        }
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_self() -> *mut std::ffi::c_void;
    fn pthread_get_qos_class_np(thread: *mut std::ffi::c_void, qos: *mut u32, relative: *mut i32) -> i32;
    fn pthread_set_qos_class_self_np(qos: u32, relative: i32) -> i32;
}
