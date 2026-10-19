//! Process-lifetime endpoint notification subscription on a dedicated MTA.
//! The UI/CPAL threads may already be STA; never change their apartments.
//! 2026-10-07 user decision: no periodic enumeration on any platform. macOS
//! 14.7.8/26.6.2 measured 111/137/1 send gaps per150s with all polling / only
//! speaker polling off / all audited polling off. Windows impact is unmeasured.
//! This observer does not enumerate or interpret IDs: CPAL still supplies the
//! exact persisted backend identifiers when the worker consumes the signal.
use std::{sync::Once, thread};
use windows::{
    Win32::{
        Foundation::PROPERTYKEY,
        Media::Audio::{
            DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl,
            MMDeviceEnumerator, eConsole, eRender,
        },
        System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize},
    },
    core::{PCWSTR, implement},
};

#[implement(IMMNotificationClient)]
struct Changed;
impl IMMNotificationClient_Impl for Changed_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
        // Existing endpoints usually report hotplug as a state transition,
        // rather than OnDeviceAdded/Removed. Record this notification as context.
        super::note_windows_notification(false);
        super::invalidate();
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        super::note_windows_notification(false);
        super::invalidate();
        Ok(())
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
        super::note_windows_notification(false);
        super::invalidate();
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> windows::core::Result<()> {
        if flow == eRender && role == eConsole {
            super::note_windows_notification(true);
        }
        super::invalidate();
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
        super::invalidate();
        Ok(())
    }
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances this thread's successful initialization after COM objects drop.
        unsafe { CoUninitialize() };
    }
}
struct Registration {
    enumerator: IMMDeviceEnumerator,
    callback: Option<IMMNotificationClient>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let callback = self.callback.as_ref().expect("registered callback");
        // SAFETY: both interfaces remain owned here, outside a notification callback.
        if let Err(error) = unsafe { self.enumerator.UnregisterEndpointNotificationCallback(callback) } {
            tracing::error!(%error, "audio endpoint notification cleanup failed");
            // Registration does not AddRef the callback. Preserve it if removal
            // failed, so a late callback can still touch the process-static atomic.
            std::mem::forget(self.callback.take());
        }
    }
}
fn observe() -> windows::core::Result<()> {
    super::windows_history::initialize();
    // SAFETY: this dedicated thread has not initialized COM or touched CPAL.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let _apartment = Apartment;
    // SAFETY: COM is initialized and the interface stays on this MTA thread.
    let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    let callback: IMMNotificationClient = Changed.into();
    // IMMNotificationClient requires nonblocking callbacks and caller-owned
    // lifetime through unregister; callbacks above only update an atomic.
    // SAFETY: Registration retains both references until after unregistration.
    unsafe { enumerator.RegisterEndpointNotificationCallback(&callback) }?;
    let _registration = Registration {
        enumerator,
        callback: Some(callback),
    };
    // Covers changes racing startup's first query before registration completed.
    super::invalidate();
    tracing::info!("audio endpoint notifications registered (MTA; process lifetime)");
    loop {
        thread::park();
    }
}
pub(super) fn start() {
    static START: Once = Once::new();
    START.call_once(|| {
        if let Err(error) = thread::Builder::new().name("oink-audio-endpoints".into()).spawn(|| {
            if let Err(error) = observe() {
                tracing::error!(%error, "audio endpoint notifications unavailable; explicit device refresh remains available");
            }
        }) {
            tracing::error!(%error, "audio endpoint notification thread unavailable; explicit device refresh remains available");
        }
    });
}
