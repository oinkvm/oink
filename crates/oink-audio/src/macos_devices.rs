//! Shared notification-invalidated HAL metadata. No AudioUnits or format probes.
//! See native/macos_devices.m for the real-machine evidence and listener lifetime.
use crate::OutputDevice;
use serde::Deserialize;
use std::{
    ffi::{CStr, c_char},
    sync::{Mutex, OnceLock},
};

unsafe extern "C" {
    fn oink_audio_devices_set_wake(wake: extern "C" fn());
    fn oink_audio_devices_generation() -> u64;
    fn oink_audio_service_generation() -> u64;
    fn oink_audio_devices_invalidate();
    fn oink_audio_devices_snapshot(error: *mut c_char, capacity: usize) -> *mut c_char;
    fn oink_audio_devices_free(value: *mut c_char);
}

#[derive(Clone, Debug, Deserialize)]
struct Device {
    uid: String,
    name: String,
    output: bool,
    is_default: bool,
}
struct Cache {
    loaded: Option<u64>,
    devices: Result<Vec<Device>, String>,
}
impl Default for Cache {
    fn default() -> Self {
        Self {
            loaded: None,
            devices: Ok(Vec::new()),
        }
    }
}
impl Cache {
    fn get(
        &mut self,
        generation: u64,
        read: impl FnOnce() -> Result<Vec<Device>, String>,
    ) -> Result<Vec<Device>, String> {
        if self.loaded != Some(generation) {
            self.devices = read();
            // A notification racing the read stays newer than this value and
            // triggers another read. Errors wait for notification/explicit retry.
            self.loaded = Some(generation);
        }
        self.devices.clone()
    }
}
/// A cheap atomic read after one-time listener registration; no enumeration.
pub fn generation() -> u64 {
    // SAFETY: process-lifetime native state; callback only increments an atomic.
    unsafe { oink_audio_devices_generation() }
}
/// Explicit settings/list request. Normal status reads must not call this.
pub fn invalidate() {
    // SAFETY: increments the same process-lifetime atomic without any HAL query.
    unsafe { oink_audio_devices_invalidate() }
}
fn read() -> Result<Vec<Device>, String> {
    let mut error = [0 as c_char; 512];
    // SAFETY: error is writable; the bridge returns an owned NUL-terminated JSON.
    let raw = unsafe { oink_audio_devices_snapshot(error.as_mut_ptr(), error.len()) };
    if raw.is_null() {
        // SAFETY: the bridge writes a bounded NUL-terminated error.
        return Err(unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned());
    }
    // SAFETY: raw is valid until freed immediately below, including on parse error.
    let parsed = serde_json::from_slice(unsafe { CStr::from_ptr(raw) }.to_bytes())
        .map_err(|e| format!("CoreAudio metadata: {e}"));
    // SAFETY: raw is the bridge-owned allocation and has not yet been freed.
    unsafe { oink_audio_devices_free(raw) };
    parsed
}
fn snapshot() -> Result<Vec<Device>, String> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(Cache::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.get(generation(), read)
}
pub fn outputs() -> Result<Vec<OutputDevice>, String> {
    Ok(snapshot()?
        .into_iter()
        .filter(|d| d.output)
        .map(|d| OutputDevice {
            id: format!("coreaudio:{}", d.uid),
            name: d.name,
            is_default: d.is_default,
        })
        .collect())
}
pub fn service_generation() -> u64 {
    // SAFETY: Process-lifetime notification counter; no device query.
    unsafe { oink_audio_service_generation() }
}

pub(super) fn set_wake(wake: extern "C" fn()) {
    // SAFETY: process-lifetime function, with no borrowed callback context.
    unsafe { oink_audio_devices_set_wake(wake) };
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unchanged_generation_never_enumerates_and_notifications_refresh_once() {
        let mut cache = Cache::default();
        let mut reads = 0;
        for generation in [1, 1, 1, 2, 2, 3, 3] {
            cache
                .get(generation, || {
                    reads += 1;
                    Ok(Vec::new())
                })
                .unwrap();
        }
        assert_eq!(reads, 3);
    }
    #[test]
    fn failed_query_waits_for_a_new_notification_or_explicit_refresh() {
        let mut cache = Cache::default();
        assert!(cache.get(4, || Err("HAL restarted".into())).is_err());
        assert!(cache.get(4, || panic!("must not poll after failure")).is_err());
        assert!(cache.get(5, || Ok(Vec::new())).is_ok());
    }
}
