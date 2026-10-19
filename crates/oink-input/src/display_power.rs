//! Read-only display sleep monitoring, independent of input capture and injection.
//!
//! Poll from the routing service timer (250 ms is sufficient), never an input callback.
//! macOS checks all online displays, including sleeping displays. Windows subscribes to
//! the console's aggregate display power state in the routing process; no window, message
//! pump, input-service IPC or foreground-window side effect is required.

use crate::InputError;

/// Read-only device observation, supplied by the input backend so simulations
/// observe their own display state instead of the host desktop.
pub trait Observation: Send {
    fn poll(&mut self) -> Option<bool>;
}
impl Observation for Monitor {
    fn poll(&mut self) -> Option<bool> {
        Monitor::poll(self)
    }
}

/// A display power subscription. Dropping it releases platform resources.
#[derive(Debug)]
pub struct Monitor {
    platform: platform::Monitor,
    last: Option<bool>,
}

impl Monitor {
    pub fn new() -> Result<Self, InputError> {
        Ok(Self {
            platform: platform::Monitor::new()?,
            last: None,
        })
    }

    /// First known state and subsequent changes: `true` means asleep, `false` awake.
    /// `None` means unchanged or unavailable. Dimming alone does not mean asleep.
    pub fn poll(&mut self) -> Option<bool> {
        changed(&mut self.last, self.platform.sample())
    }
}

fn changed(last: &mut Option<bool>, sample: Option<bool>) -> Option<bool> {
    let sample = sample?;
    if *last == Some(sample) {
        return None;
    }
    *last = Some(sample);
    Some(sample)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::InputError;
    use objc2_core_graphics::{CGDisplayIsAsleep, CGError, CGGetOnlineDisplayList};

    #[derive(Debug)]
    pub(super) struct Monitor;
    impl Monitor {
        pub(super) fn new() -> Result<Self, InputError> {
            Ok(Self)
        }
        pub(super) fn sample(&self) -> Option<bool> {
            // A full buffer may indicate truncation during a display hotplug; fail closed.
            let mut displays = [0u32; 64];
            let mut count = 0;
            // SAFETY: correctly sized writable display buffer and count.
            let result = unsafe { CGGetOnlineDisplayList(displays.len() as u32, displays.as_mut_ptr(), &mut count) };
            if result != CGError::Success || count == 0 || count as usize >= displays.len() {
                return None;
            }
            Some(displays[..count as usize].iter().all(|&id| CGDisplayIsAsleep(id)))
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::InputError;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Power::{
        DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, POWERBROADCAST_SETTING, PowerSettingRegisterNotification,
        PowerSettingUnregisterNotification,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_POWERSETTINGCHANGE};
    use windows::core::GUID;

    const DISPLAY_STATE: GUID = GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);
    const UNKNOWN: u32 = u32::MAX;
    // Callback contexts are unique integer tokens, never dereferenced pointers. Removing
    // a subscription is safe even if a callback races with unregister (or it fails).
    static STATES: OnceLock<Mutex<HashMap<usize, Arc<AtomicU32>>>> = OnceLock::new();
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    fn states() -> &'static Mutex<HashMap<usize, Arc<AtomicU32>>> {
        STATES.get_or_init(Default::default)
    }

    #[derive(Debug)]
    pub(super) struct Monitor {
        registration: usize,
        token: usize,
        state: Arc<AtomicU32>,
    }
    impl Monitor {
        pub(super) fn new() -> Result<Self, InputError> {
            let token = NEXT.fetch_add(1, Ordering::Relaxed);
            let state = Arc::new(AtomicU32::new(UNKNOWN));
            states()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(token, state.clone());
            let params = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(callback),
                Context: token as *mut c_void,
            };
            let mut registration = std::ptr::null_mut();
            // SAFETY: parameters remain valid through registration; callback context is
            // an opaque integer looked up in STATES, including synchronous initial calls.
            let result = unsafe {
                PowerSettingRegisterNotification(
                    &DISPLAY_STATE,
                    DEVICE_NOTIFY_CALLBACK,
                    HANDLE((&params as *const DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS).cast_mut().cast()),
                    &mut registration,
                )
            };
            if result.0 != 0 {
                states().lock().unwrap_or_else(|e| e.into_inner()).remove(&token);
                return Err(InputError::Platform(format!(
                    "PowerSettingRegisterNotification: {}",
                    result.0
                )));
            }
            Ok(Self {
                registration: registration as usize,
                token,
                state,
            })
        }
        pub(super) fn sample(&self) -> Option<bool> {
            match self.state.load(Ordering::Relaxed) {
                0 => Some(true),
                1 | 2 => Some(false),
                _ => None,
            }
        }
    }
    impl Drop for Monitor {
        fn drop(&mut self) {
            states().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.token);
            // SAFETY: handle belongs to this monitor and is unregistered exactly once.
            let result = unsafe { PowerSettingUnregisterNotification(HPOWERNOTIFY(self.registration as isize)) };
            if result.0 != 0 {
                tracing::warn!(code = result.0, "PowerSettingUnregisterNotification failed");
            }
        }
    }
    unsafe extern "system" fn callback(context: *const c_void, kind: u32, setting: *const c_void) -> u32 {
        if kind != PBT_POWERSETTINGCHANGE || setting.is_null() {
            return 0;
        }
        // SAFETY: for this notification Windows supplies POWERBROADCAST_SETTING, valid
        // throughout the callback. Its variable-length Data is checked before reading.
        let setting = unsafe { &*setting.cast::<POWERBROADCAST_SETTING>() };
        if setting.PowerSetting != DISPLAY_STATE || setting.DataLength != 4 {
            return 0;
        }
        // SAFETY: Windows guarantees DataLength bytes; Data may not be u32 aligned.
        let value = unsafe { setting.Data.as_ptr().cast::<u32>().read_unaligned() };
        if let Some(state) = states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(context as usize))
        {
            state.store(value, Ordering::Relaxed);
        }
        0
    }

    #[cfg(all(test, debug_assertions))]
    mod tests {
        use super::*;

        #[repr(C)]
        struct Notification {
            guid: GUID,
            length: u32,
            value: u32,
        }

        #[test]
        fn callback_filters_short_notifications_and_retired_contexts() {
            let token = NEXT.fetch_add(1, Ordering::Relaxed);
            let state = Arc::new(AtomicU32::new(UNKNOWN));
            states().lock().unwrap().insert(token, state.clone());
            let mut packet = Notification {
                guid: DISPLAY_STATE,
                length: 4,
                value: 0,
            };
            let deliver = |packet: &Notification, kind| {
                // SAFETY: packet has the native header and four readable data bytes;
                // context is only a registry token, never dereferenced.
                unsafe { callback(token as *const c_void, kind, (packet as *const Notification).cast()) };
            };
            deliver(&packet, 0);
            assert_eq!(state.load(Ordering::Relaxed), UNKNOWN);
            packet.length = 1;
            deliver(&packet, PBT_POWERSETTINGCHANGE);
            assert_eq!(state.load(Ordering::Relaxed), UNKNOWN);
            packet.length = 4;
            for value in [0, 1, 2] {
                packet.value = value;
                deliver(&packet, PBT_POWERSETTINGCHANGE);
                assert_eq!(state.load(Ordering::Relaxed), value);
            }
            states().lock().unwrap().remove(&token);
            packet.value = 0;
            deliver(&packet, PBT_POWERSETTINGCHANGE);
            assert_eq!(state.load(Ordering::Relaxed), 2);
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::InputError;
    #[derive(Debug)]
    pub(super) struct Monitor;
    impl Monitor {
        pub(super) fn new() -> Result<Self, InputError> {
            Err(InputError::Unsupported)
        }
        pub(super) fn sample(&self) -> Option<bool> {
            None
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::changed;
    #[test]
    fn emits_initial_state_and_changes_but_not_unknown_or_repeated_states() {
        let mut last = None;
        assert_eq!(changed(&mut last, None), None);
        assert_eq!(changed(&mut last, Some(false)), Some(false));
        assert_eq!(changed(&mut last, Some(false)), None);
        assert_eq!(changed(&mut last, Some(true)), Some(true));
        assert_eq!(changed(&mut last, None), None);
        assert_eq!(changed(&mut last, Some(true)), None);
        assert_eq!(changed(&mut last, Some(false)), Some(false));
    }
}
