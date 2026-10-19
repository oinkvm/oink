//! Read-only system suspend/resume notifications, independent of display sleep.
//! No power assertions, no system setting changes and no automatic capture restart.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub epoch: u64,
    pub sleeping: bool,
}

#[derive(Debug)]
pub struct Monitor(platform::Monitor);
impl Monitor {
    pub fn new() -> Result<Self, String> {
        platform::Monitor::new().map(Self)
    }
    pub fn state(&self) -> State {
        self.0.state()
    }
    /// Called only after the actor has dropped capture/playback and all leases.
    pub fn acknowledge(&self, epoch: u64) {
        self.0.acknowledge(epoch);
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::State;
    use std::ffi::c_void;
    unsafe extern "C" {
        fn oink_audio_power_start() -> *mut c_void;
        fn oink_audio_power_state(raw: *mut c_void, asleep: *mut bool) -> u64;
        fn oink_audio_power_ack(raw: *mut c_void, epoch: u64);
        fn oink_audio_power_stop(raw: *mut c_void);
    }
    #[derive(Debug)]
    pub struct Monitor(*mut c_void);
    impl Monitor {
        pub fn new() -> Result<Self, String> {
            // SAFETY: returns an owned observer object; no callbacks into Rust.
            let raw = unsafe { oink_audio_power_start() };
            if raw.is_null() {
                Err("could not observe system sleep/wake".into())
            } else {
                Ok(Self(raw))
            }
        }
        pub fn state(&self) -> State {
            let mut sleeping = false;
            // SAFETY: owned live monitor and valid bool result pointer.
            let epoch = unsafe { oink_audio_power_state(self.0, &mut sleeping) };
            State { epoch, sleeping }
        }
        pub fn acknowledge(&self, epoch: u64) {
            // SAFETY: monitor remains alive throughout the call.
            unsafe {
                oink_audio_power_ack(self.0, epoch);
            }
        }
    }
    impl Drop for Monitor {
        fn drop(&mut self) {
            // SAFETY: releases the owned object once, unregistering both observers.
            unsafe {
                oink_audio_power_stop(self.0);
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::State;
    use std::{
        collections::HashMap,
        ffi::c_void,
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
    };
    use windows::Win32::{
        Foundation::HANDLE,
        System::Power::{
            DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, PowerRegisterSuspendResumeNotification,
            PowerUnregisterSuspendResumeNotification,
        },
        UI::WindowsAndMessaging::{
            DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
        },
    };
    static STATES: OnceLock<Mutex<HashMap<usize, Arc<AtomicU64>>>> = OnceLock::new();
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    fn states() -> &'static Mutex<HashMap<usize, Arc<AtomicU64>>> {
        STATES.get_or_init(Default::default)
    }
    #[derive(Debug)]
    pub struct Monitor {
        registration: usize,
        token: usize,
        state: Arc<AtomicU64>,
    }
    impl Monitor {
        pub fn new() -> Result<Self, String> {
            let token = NEXT.fetch_add(1, Ordering::Relaxed);
            let state = Arc::new(AtomicU64::new(2));
            states()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(token, state.clone());
            let params = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(changed),
                Context: token as *mut c_void,
            };
            let mut registration = std::ptr::null_mut();
            // SAFETY: context is an integer registry key; parameters are valid for registration.
            let result = unsafe {
                PowerRegisterSuspendResumeNotification(
                    DEVICE_NOTIFY_CALLBACK,
                    HANDLE((&params as *const DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS).cast_mut().cast()),
                    &mut registration,
                )
            };
            if result.0 != 0 {
                states().lock().unwrap_or_else(|e| e.into_inner()).remove(&token);
                return Err(format!("PowerRegisterSuspendResumeNotification: {}", result.0));
            }
            Ok(Self {
                registration: registration as usize,
                token,
                state,
            })
        }
        pub fn state(&self) -> State {
            let value = self.state.load(Ordering::Acquire);
            State {
                epoch: value >> 1,
                sleeping: value & 1 != 0,
            }
        }
        pub fn acknowledge(&self, _: u64) {}
    }
    impl Drop for Monitor {
        fn drop(&mut self) {
            states().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.token);
            // SAFETY: owned registration, unregistered exactly once; late contexts do not resolve.
            let result = unsafe { PowerUnregisterSuspendResumeNotification(HPOWERNOTIFY(self.registration as isize)) };
            if result.0 != 0 {
                tracing::warn!(code = result.0, "audio suspend notification cleanup failed");
            }
        }
    }
    unsafe extern "system" fn changed(context: *const c_void, kind: u32, _: *const c_void) -> u32 {
        let sleeping = match kind {
            PBT_APMSUSPEND => true,
            PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => false,
            _ => return 0,
        };
        if let Some(state) = states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(context as usize))
        {
            let mut old = state.load(Ordering::Acquire);
            loop {
                let next = ((old & !1).wrapping_add(2)) | u64::from(sleeping);
                match state.compare_exchange_weak(old, next, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => {
                        crate::device_notifications::notify();
                        break;
                    }
                    Err(current) => old = current,
                }
            }
        }
        0
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::State;
    #[derive(Debug)]
    pub struct Monitor;
    impl Monitor {
        pub fn new() -> Result<Self, String> {
            Err("system sleep monitoring unsupported".into())
        }
        pub fn state(&self) -> State {
            State {
                sleeping: true,
                epoch: 0,
            }
        }
        pub fn acknowledge(&self, _: u64) {}
    }
}
