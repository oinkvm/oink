//! Device-free integration evidence. Never enabled in product builds.
use std::{marker::PhantomData, rc::Rc};
#[derive(Debug)]
pub struct Priority {
    #[cfg(target_os = "macos")]
    raw: *mut std::ffi::c_void,
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
    _affine: PhantomData<Rc<()>>,
}
impl Priority {
    pub fn enter(period_ns: u64) -> Result<Self, i32> {
        #[cfg(target_os = "macos")]
        let raw = {
            let mut error = 0;
            // SAFETY: scheduling setup, before the measured callback work.
            let raw = unsafe { oink_pull_benchmark_enter(period_ns, &mut error) };
            if raw.is_null() {
                return Err(error);
            }
            raw
        };
        #[cfg(windows)]
        let handle = {
            use windows::{Win32::System::Threading::AvSetMmThreadCharacteristicsW, core::w};
            let mut index = 0;
            // SAFETY: balanced on this thread by Drop.
            unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) }.map_err(|error| error.code().0)?
        };
        #[cfg(not(target_os = "macos"))]
        let _ = period_ns;
        Ok(Self {
            #[cfg(target_os = "macos")]
            raw,
            #[cfg(windows)]
            handle,
            _affine: PhantomData,
        })
    }
}
impl Drop for Priority {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        // SAFETY: unique thread-affine registration.
        unsafe {
            oink_pull_benchmark_leave(self.raw);
        }
        #[cfg(windows)]
        // SAFETY: unique registration on this same thread.
        unsafe {
            let _ = windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.handle);
        }
    }
}
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn oink_pull_benchmark_enter(period_ns: u64, error: *mut i32) -> *mut std::ffi::c_void;
    fn oink_pull_benchmark_leave(raw: *mut std::ffi::c_void);
    fn oink_pull_audit_symbol(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
}
/// Optional process-wide C allocator interposer, loaded before test startup.
#[derive(Clone, Copy, Debug)]
pub struct CHeapAudit {
    begin: unsafe extern "C" fn(),
    end: unsafe extern "C" fn() -> u64,
}
impl CHeapAudit {
    pub fn load() -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: symbols are the fixed ABI of our evidence interposer.
            unsafe {
                let begin = oink_pull_audit_symbol(c"oink_heap_audit_begin".as_ptr());
                let end = oink_pull_audit_symbol(c"oink_heap_audit_end".as_ptr());
                if begin.is_null() || end.is_null() {
                    return None;
                }
                Some(Self {
                    begin: std::mem::transmute::<*mut std::ffi::c_void, unsafe extern "C" fn()>(begin),
                    end: std::mem::transmute::<*mut std::ffi::c_void, unsafe extern "C" fn() -> u64>(end),
                })
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }
    pub fn measure(self, work: impl FnOnce()) -> u64 {
        // SAFETY: ABI resolved during cold setup; counters use prewarmed TLS.
        unsafe {
            (self.begin)();
        }
        work();
        // SAFETY: pairs with begin above; the loaded audit hook has the declared
        // no-argument ABI and remains loaded for this process's lifetime.
        unsafe { (self.end)() }
    }
}
