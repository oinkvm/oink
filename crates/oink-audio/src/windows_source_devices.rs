//! Capture-scoped identity observer. Never opens or controls the new endpoint.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use windows::{
    Win32::{
        Foundation::PROPERTYKEY,
        Media::Audio::{
            DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl,
            PKEY_AudioEngine_DeviceFormat, eConsole, eRender,
        },
    },
    core::{PCWSTR, implement},
};

#[derive(Default, Debug)]
pub(super) struct Changes {
    pub default: AtomicBool,
    pub format: AtomicBool,
}

#[implement(IMMNotificationClient)]
struct Callback {
    endpoint: Vec<u16>,
    changes: Arc<Changes>,
}
impl Callback {
    fn matches(&self, endpoint: &PCWSTR) -> bool {
        // SAFETY: MMDevice supplies a terminated ID valid for this callback.
        // A null default means that no replacement render endpoint exists.
        !endpoint.is_null() && unsafe { endpoint.as_wide() } == self.endpoint
    }
}
impl IMMNotificationClient_Impl for Callback_Impl {
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, endpoint: &PCWSTR) -> windows::core::Result<()> {
        // Windows reports each role separately. Loopback/CPAL use eConsole;
        // communications-only changes and duplicate notifications are not a
        // source device change. Latch identity against this capture, not a
        // mutable "current default" baseline: A→B→A still ends the old capture.
        // Callbacks only publish atomics/wake; COM, IO and restoration happen
        // outside MMDevice callbacks. No real hotplug test in this change.
        if flow == eRender && role == eConsole && !self.matches(endpoint) {
            self.changes.default.store(true, Ordering::Release);
            crate::device_notifications::notify();
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, endpoint: &PCWSTR, key: &PROPERTYKEY) -> windows::core::Result<()> {
        if *key == PKEY_AudioEngine_DeviceFormat && self.matches(endpoint) {
            self.changes.format.store(true, Ordering::Release);
            crate::device_notifications::notify();
        }
        Ok(())
    }
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
}

pub(super) struct Subscription {
    enumerator: IMMDeviceEnumerator,
    callback: Option<IMMNotificationClient>,
}
impl Subscription {
    pub fn new(enumerator: IMMDeviceEnumerator, endpoint: &str, changes: Arc<Changes>) -> Result<Self, String> {
        let callback: IMMNotificationClient = Callback {
            endpoint: endpoint.encode_utf16().collect(),
            changes,
        }
        .into();
        // SAFETY: capture's MTA owns the enumerator; subscription retains the
        // callback through unregistration (registration itself does not AddRef).
        unsafe { enumerator.RegisterEndpointNotificationCallback(&callback) }
            .map_err(|e| format!("register source endpoint identity observer: {e}"))?;
        Ok(Self {
            enumerator,
            callback: Some(callback),
        })
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        // SAFETY: called on the owning MTA after capture IO/restoration, never
        // inside a native callback. Keep callback alive if unregister fails.
        if let Err(error) = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(self.callback.as_ref().unwrap())
        } {
            tracing::error!(%error, "source endpoint identity observer cleanup failed");
            std::mem::forget(self.callback.take());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn callback() -> (IMMNotificationClient, Arc<Changes>) {
        let changes = Arc::new(Changes::default());
        (
            Callback {
                endpoint: "endpoint-a".encode_utf16().collect(),
                changes: changes.clone(),
            }
            .into(),
            changes,
        )
    }
    #[test]
    fn default_identity_is_sticky_and_ignores_other_roles_and_input_devices() {
        let (callback, changes) = callback();
        // SAFETY: in-process fake notifications; no COM initialization, native
        // endpoint access, system setting changes, or input injection.
        unsafe {
            callback
                .OnDefaultDeviceChanged(eRender, eConsole, windows::core::w!("endpoint-a"))
                .unwrap();
            callback
                .OnDefaultDeviceChanged(
                    eRender,
                    windows::Win32::Media::Audio::eCommunications,
                    windows::core::w!("endpoint-b"),
                )
                .unwrap();
            callback
                .OnDefaultDeviceChanged(
                    windows::Win32::Media::Audio::eCapture,
                    eConsole,
                    windows::core::w!("endpoint-b"),
                )
                .unwrap();
            assert!(!changes.default.load(Ordering::Acquire));
            callback
                .OnDefaultDeviceChanged(eRender, eConsole, windows::core::w!("endpoint-b"))
                .unwrap();
            callback
                .OnDefaultDeviceChanged(eRender, eConsole, windows::core::w!("endpoint-a"))
                .unwrap();
        }
        assert!(changes.default.load(Ordering::Acquire));
    }
    #[test]
    fn missing_default_is_a_change_but_same_endpoint_format_is_only_a_restart() {
        let (callback, changes) = callback();
        // SAFETY: fake endpoint IDs/property callbacks; does not touch hardware.
        unsafe {
            callback
                .OnPropertyValueChanged(windows::core::w!("endpoint-b"), PKEY_AudioEngine_DeviceFormat)
                .unwrap();
            assert!(!changes.format.load(Ordering::Acquire));
            callback
                .OnPropertyValueChanged(windows::core::w!("endpoint-a"), PKEY_AudioEngine_DeviceFormat)
                .unwrap();
            assert!(changes.format.load(Ordering::Acquire));
            assert!(!changes.default.load(Ordering::Acquire));
            callback
                .OnDefaultDeviceChanged(eRender, eConsole, PCWSTR::null())
                .unwrap();
        }
        assert!(changes.default.load(Ordering::Acquire));
    }
}
