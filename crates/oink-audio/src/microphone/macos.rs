//! Read-only TCC and HAL queries. Neither function opens an audio stream.
use super::{MAC_FEED_UID, MAC_INPUT_UID, Permission};
use objc2::{class, msg_send, runtime::AnyObject};
use std::{
    collections::HashMap,
    ffi::{CString, c_void},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVMediaTypeAudio: *const AnyObject;
}

pub fn permission() -> Permission {
    // SAFETY: AVFoundation's immutable AVMediaTypeAudio constant and documented
    // class method signature. authorizationStatus... never requests permission.
    let value: isize = unsafe { msg_send![class!(AVCaptureDevice), authorizationStatusForMediaType: AVMediaTypeAudio] };
    match value {
        0 => Permission::NotDetermined,
        1 => Permission::Restricted,
        2 => Permission::Denied,
        3 => Permission::Granted,
        _ => Permission::Restricted,
    }
}

#[repr(C)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}
#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioObjectAddPropertyListener(
        object: u32,
        address: *const Address,
        listener: unsafe extern "C" fn(u32, u32, *const Address, *mut c_void) -> i32,
        context: *mut c_void,
    ) -> i32;
    fn AudioObjectRemovePropertyListener(
        object: u32,
        address: *const Address,
        listener: unsafe extern "C" fn(u32, u32, *const Address, *mut c_void) -> i32,
        context: *mut c_void,
    ) -> i32;
    fn oink_microphone_request_permission() -> bool;
}

pub fn request_permission() -> Result<(), String> {
    // SAFETY: no borrowed data; the native bridge dispatches the explicit request
    // to the application's main queue and first verifies its usage description.
    if unsafe { oink_microphone_request_permission() } {
        Ok(())
    } else {
        Err("the application host is missing NSMicrophoneUsageDescription".into())
    }
}

// A callback context is an opaque integer token, never a dereferenced allocation.
// Removal from the registry makes late callbacks safe even if HAL removal fails.
struct Listener {
    epoch: Arc<AtomicU64>,
    wake: Arc<dyn Fn() + Send + Sync>,
}
static LISTENERS: OnceLock<Mutex<HashMap<usize, Arc<Listener>>>> = OnceLock::new();
static NEXT_LISTENER: AtomicUsize = AtomicUsize::new(1);
fn listeners() -> &'static Mutex<HashMap<usize, Arc<Listener>>> {
    LISTENERS.get_or_init(Default::default)
}
unsafe extern "C" fn demand_changed(_: u32, _: u32, _: *const Address, context: *mut c_void) -> i32 {
    let listener = listeners()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(context as usize))
        .cloned();
    if let Some(listener) = listener {
        listener.epoch.fetch_add(1, Ordering::AcqRel);
        (listener.wake)();
    }
    0
}

// Observe other applications, not this backend's own IOProc. On macOS 26.6.2
// with the installed v1 driver, read-only probes saw goin=0 and gone=1 while
// another app consumed Oink Microphone; using goin therefore never requested
// remote capture. HAL documents gone as DeviceIsRunningSomewhere. Subscribe and
// query the visible input with the same selector; the hidden feed never counts
// as demand. This neither opens IO nor changes the user's default input.
const CONSUMER_RUNNING: &[u8; 4] = b"gone";

/// Subscribe before the first running-state query; also re-resolve UIDs after
/// HAL restarts/hotplug. Only the visible input's StartIO/StopIO creates demand.
#[derive(Debug)]
pub struct DemandMonitor {
    token: usize,
    epoch: Arc<AtomicU64>,
    input: Option<u32>,
}
impl DemandMonitor {
    pub fn new() -> Result<Self, String> {
        Self::with_wake(Arc::new(|| {}))
    }
    pub fn revision(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
    pub fn with_wake(wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self, String> {
        let token = NEXT_LISTENER.fetch_add(1, Ordering::Relaxed);
        let epoch = Arc::new(AtomicU64::new(1));
        listeners().lock().unwrap_or_else(|e| e.into_inner()).insert(
            token,
            Arc::new(Listener {
                epoch: epoch.clone(),
                wake,
            }),
        );
        // SAFETY: token is not dereferenced; callback and registry are process-long.
        let result =
            unsafe { AudioObjectAddPropertyListener(1, &address(b"dev#"), demand_changed, token as *mut c_void) };
        if result != 0 {
            listeners().lock().unwrap_or_else(|e| e.into_inner()).remove(&token);
            return Err(format!("HAL device listener failed: {result}"));
        }
        Ok(Self {
            token,
            epoch,
            input: None,
        })
    }
    fn replace_input(&mut self, next: Option<u32>) -> Result<(), String> {
        if self.input == next {
            return Ok(());
        }
        if let Some(old) = self.input.take() {
            // SAFETY: registration belongs to this token; old IDs can disappear
            // on HAL restart. Retired callbacks still only touch the live registry.
            unsafe {
                AudioObjectRemovePropertyListener(
                    old,
                    &address(CONSUMER_RUNNING),
                    demand_changed,
                    self.token as *mut c_void,
                );
            }
        }
        if let Some(next) = next {
            // SAFETY: fixed property, lifetime-independent callback/token.
            let result = unsafe {
                AudioObjectAddPropertyListener(
                    next,
                    &address(CONSUMER_RUNNING),
                    demand_changed,
                    self.token as *mut c_void,
                )
            };
            if result != 0 {
                return Err(format!("HAL input listener failed: {result}"));
            }
            self.input = Some(next);
        }
        self.epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    pub fn sample(&mut self) -> super::Demand {
        let sample = (|| {
            let id = device_id(MAC_INPUT_UID)?;
            self.replace_input(id)?;
            let id = id.ok_or("Oink Microphone is not loaded")?;
            if device_id(MAC_FEED_UID)?.is_none() {
                return Err("Oink Microphone Feed is not loaded".into());
            }
            running(id)
        })();
        match sample {
            Ok(active) => super::Demand {
                active,
                epoch: self.epoch.load(Ordering::Acquire),
                error: None,
            },
            Err(error) => super::Demand {
                active: false,
                epoch: self.epoch.load(Ordering::Acquire),
                error: Some(error),
            },
        }
    }
}
impl Drop for DemandMonitor {
    fn drop(&mut self) {
        listeners()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
        // SAFETY: registrations owned by this monitor, context no longer resolves.
        unsafe {
            if let Some(input) = self.input {
                AudioObjectRemovePropertyListener(
                    input,
                    &address(CONSUMER_RUNNING),
                    demand_changed,
                    self.token as *mut c_void,
                );
            }
            AudioObjectRemovePropertyListener(1, &address(b"dev#"), demand_changed, self.token as *mut c_void);
        }
    }
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(allocator: *const c_void, text: *const i8, encoding: u32) -> *const c_void;
    fn CFRelease(object: *const c_void);
    fn CFStringGetCString(string: *const c_void, buffer: *mut i8, size: isize, encoding: u32) -> u8;
}

fn address(selector: &[u8; 4]) -> Address {
    Address {
        selector: u32::from_be_bytes(*selector),
        scope: u32::from_be_bytes(*b"glob"),
        element: 0,
    }
}

pub fn device_id(uid: &str) -> Result<Option<u32>, String> {
    let text = CString::new(uid).map_err(|e| e.to_string())?;
    // SAFETY: valid NUL-terminated UTF-8, default CF allocator, owned +1 string.
    let string = unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), 0x08000100) };
    if string.is_null() {
        return Err("could not allocate device UID".into());
    }
    let mut result = 0_u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: SDK's TranslateUIDToDevice takes a CFStringRef qualifier and UInt32
    // result. All pointers and lengths match; string lives across the call.
    let status = unsafe {
        AudioObjectGetPropertyData(
            1,
            &address(b"uidd"),
            size_of_val(&string) as u32,
            (&string as *const *const c_void).cast(),
            &mut size,
            (&mut result as *mut u32).cast(),
        )
    };
    // SAFETY: release the owned CFString exactly once, after its last use.
    unsafe {
        CFRelease(string);
    }
    if status != 0 {
        return Err(format!("HAL UID lookup failed: {status}"));
    }
    Ok((result != 0).then_some(result))
}

fn running(id: u32) -> Result<bool, String> {
    running_with(id, |id, address| {
        let mut result = 0_u32;
        let mut size = size_of::<u32>() as u32;
        // SAFETY: device-running property is UInt32, with no qualifier.
        let status = unsafe {
            AudioObjectGetPropertyData(
                id,
                address,
                0,
                std::ptr::null(),
                &mut size,
                (&mut result as *mut u32).cast(),
            )
        };
        if status != 0 {
            Err(format!("HAL running query failed: {status}"))
        } else {
            Ok(result)
        }
    })
}
fn running_with(id: u32, query: impl FnOnce(u32, &Address) -> Result<u32, String>) -> Result<bool, String> {
    query(id, &address(CONSUMER_RUNNING)).map(|value| value != 0)
}

// fwvn is implemented by the driver but not exposed by HAL on the observed
// macOS 14.7.8 installation. AcID is explicitly declared as a custom CFString
// for host/client marshalling; do not fall back to disk metadata, which cannot
// prove which binary coreaudiod loaded. See mic-plugin-installation.md.
fn driver_identity(id: u32) -> Result<String, String> {
    let mut string: *const c_void = std::ptr::null();
    let mut size = size_of_val(&string) as u32;
    // SAFETY: AcID is declared by the driver as an owned CFStringRef returned by CoreAudio.
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            &address(b"AcID"),
            0,
            std::ptr::null(),
            &mut size,
            (&mut string as *mut *const c_void).cast(),
        )
    };
    if status != 0 || string.is_null() {
        return Err(format!(
            "HAL driver identity query failed (AcID, device {id}): {status}"
        ));
    }
    let mut bytes = [0_u8; 256];
    // SAFETY: live CFString, writable buffer of exactly the declared size; UTF-8.
    let converted = unsafe { CFStringGetCString(string, bytes.as_mut_ptr().cast(), bytes.len() as isize, 0x08000100) };
    // SAFETY: balance the owned property reference exactly once.
    unsafe {
        CFRelease(string);
    }
    if converted == 0 {
        return Err("HAL driver identity is not a short UTF-8 string".into());
    }
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    String::from_utf8(bytes[..end].to_vec()).map_err(|error| error.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginStatus {
    pub input_device: Option<u32>,
    pub feed_device: Option<u32>,
    /// None means missing. Errors remain errors, never interpreted as demand.
    pub consumer_running: Option<bool>,
    pub input_git_sha: Option<String>,
    pub feed_git_sha: Option<String>,
    pub input_plugin_version: Option<String>,
    pub feed_plugin_version: Option<String>,
}

/// Query the visible input, NOT the hidden feed. Rendering into the feed alone
/// must not create receiver demand. Stage two should listen + recheck this property.
pub fn plugin_status() -> Result<PluginStatus, String> {
    let input_device = device_id(MAC_INPUT_UID)?;
    let feed_device = device_id(MAC_FEED_UID)?;
    let (input_plugin_version, input_git_sha) = decode_driver_identity(input_device.map(driver_identity).transpose()?);
    let (feed_plugin_version, feed_git_sha) = decode_driver_identity(feed_device.map(driver_identity).transpose()?);
    Ok(PluginStatus {
        input_device,
        feed_device,
        consumer_running: input_device.map(running).transpose()?,
        input_git_sha,
        feed_git_sha,
        input_plugin_version,
        feed_plugin_version,
    })
}

// Only the current version-1 driver identity is recognized.
fn decode_driver_identity(value: Option<String>) -> (Option<String>, Option<String>) {
    let Some(value) = value else { return (None, None) };
    if let Some((version, git_sha)) = value.strip_prefix("v1:").and_then(|s| s.split_once(':'))
        && version.len() == 64
        && version.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return (Some(version.into()), Some(git_sha.into()));
    }
    (None, None)
}

#[cfg(test)]
mod identity_tests {
    use super::decode_driver_identity;
    #[test]
    fn loaded_driver_version_and_provenance_are_independent() {
        let version = "a".repeat(64);
        assert_eq!(
            decode_driver_identity(Some(format!("v1:{version}:old-app-sha"))),
            (Some(version), Some("old-app-sha".into()))
        );
        assert_eq!(decode_driver_identity(Some("legacy-app-sha".into())), (None, None));
        for invalid in [
            "v2:abc:def".to_owned(),
            "v1:abc:def".to_owned(),
            format!("v1:{}:build", "g".repeat(64)),
        ] {
            assert_eq!(decode_driver_identity(Some(invalid)), (None, None));
        }
        assert_eq!(decode_driver_identity(None), (None, None));
    }
}

#[cfg(test)]
mod demand_tests {
    use super::*;
    #[test]
    fn microphone_demand_observes_recording_in_another_process() {
        let visible_input = 42;
        // HAL reports this observer's IO as stopped even when another app is
        // recording the visible input. The hidden feed is deliberately idle.
        let active = running_with(visible_input, |id, address| {
            assert_eq!(id, visible_input);
            assert_eq!(address.scope, u32::from_be_bytes(*b"glob"));
            Ok(u32::from(address.selector == u32::from_be_bytes(*b"gone")))
        })
        .unwrap();
        assert!(
            active,
            "an external consumer must create microphone demand without starting IO in this process"
        );
        assert!(!running_with(visible_input, |_, _| Ok(0)).unwrap());
        assert_eq!(
            running_with(visible_input, |_, _| Err("device disappeared".into())),
            Err("device disappeared".into())
        );
    }
}
