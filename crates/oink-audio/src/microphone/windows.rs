//! Observe actual capture sessions on VB-CABLE's recording endpoint. Never open
//! an IAudioClient, change an endpoint or infer demand from our render session.
use super::Demand;
use cpal::traits::{DeviceTrait, HostTrait};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
};
use windows::{
    Win32::{
        Media::Audio::{
            AudioSessionDisconnectReason, AudioSessionState, AudioSessionStateActive, AudioSessionStateExpired,
            DEVICE_STATE_ACTIVE, IAudioSessionControl, IAudioSessionControl2, IAudioSessionEvents,
            IAudioSessionEvents_Impl, IAudioSessionManager2, IAudioSessionNotification, IAudioSessionNotification_Impl,
            IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
        },
        System::Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
        },
    },
    core::{AgileReference, GUID, Interface, PCWSTR, Ref, implement},
};

#[derive(Debug)]
/// Keep the microphone worker in MTA before any CPAL enumeration. CPAL 0.17
/// otherwise installs STA in thread-local storage; later MTA initialization
/// fails with RPC_E_CHANGED_MODE and session notifications cannot be registered.
/// CPAL explicitly supports a pre-existing MTA. This touches only the dedicated
/// microphone thread, never the UI apartment. Verified on Windows 11.
pub struct WorkerApartment(std::marker::PhantomData<*mut ()>);
impl WorkerApartment {
    pub fn new() -> Result<Self, String> {
        // SAFETY: constructed/dropped on the microphone worker, after COM objects.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| e.to_string())?;
        Ok(Self(std::marker::PhantomData))
    }
}
impl Drop for WorkerApartment {
    fn drop(&mut self) {
        // SAFETY: balances this thread's successful initialization, after interfaces drop.
        unsafe {
            CoUninitialize();
        }
    }
}

#[implement(IAudioSessionNotification)]
struct Created {
    epoch: Arc<AtomicU64>,
    wake: Arc<dyn Fn() + Send + Sync>,
    tx: SyncSender<AgileReference<IAudioSessionControl>>,
    failed: Arc<AtomicBool>,
}
impl IAudioSessionNotification_Impl for Created_Impl {
    fn OnSessionCreated(&self, session: Ref<'_, IAudioSessionControl>) -> windows::core::Result<()> {
        // Marshal through an agile reference; raw COM pointers never cross threads.
        let result = session.as_ref().and_then(|s| AgileReference::new(s).ok());
        if result.is_none_or(|s| self.tx.try_send(s).is_err()) {
            self.failed.store(true, Ordering::Release);
        }
        self.epoch.fetch_add(1, Ordering::AcqRel);
        (self.wake)();
        Ok(())
    }
}

#[implement(IAudioSessionEvents)]
struct Events {
    epoch: Arc<AtomicU64>,
    wake: Arc<dyn Fn() + Send + Sync>,
}
impl IAudioSessionEvents_Impl for Events_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(&self, _: f32, _: windows::core::BOOL, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnChannelVolumeChanged(&self, _: u32, _: *const f32, _: u32, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, _: AudioSessionState) -> windows::core::Result<()> {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        (self.wake)();
        Ok(())
    }
    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> windows::core::Result<()> {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        (self.wake)();
        Ok(())
    }
}

#[derive(Debug)]
struct Watch {
    control: IAudioSessionControl,
    events: IAudioSessionEvents,
}
impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: interface/callback belong to this registration and live through removal.
        if let Err(error) = unsafe { self.control.UnregisterAudioSessionNotification(&self.events) } {
            tracing::warn!(%error, "microphone session observer cleanup failed");
        }
    }
}

pub struct DemandMonitor {
    wake: Arc<dyn Fn() + Send + Sync>,
    manager: IAudioSessionManager2,
    created: IAudioSessionNotification,
    device: IMMDevice,
    sessions: BTreeMap<String, Watch>,
    rx: Receiver<AgileReference<IAudioSessionControl>>,
    failed: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    // Fields drop in declaration order: COM must outlive every interface.
    _apartment: WorkerApartment,
}
impl std::fmt::Debug for DemandMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DemandMonitor").finish_non_exhaustive()
    }
}
impl DemandMonitor {
    pub fn new() -> Result<Self, String> {
        Self::with_wake(Arc::new(|| {}))
    }
    pub fn revision(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
    pub fn with_wake(wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self, String> {
        let apartment = WorkerApartment::new()?;
        let mut matches = Vec::new();
        for device in cpal::default_host().input_devices().map_err(|e| e.to_string())? {
            if crate::device_name(&device.description().map_err(|e| e.to_string())?)
                == "CABLE Output (VB-Audio Virtual Cable)"
            {
                matches.push(device.id().map_err(|e| e.to_string())?.to_string());
            }
        }
        if matches.len() != 1 {
            return Err("VB-CABLE recording endpoint is missing or ambiguous".into());
        }
        let raw = matches[0]
            .strip_prefix("wasapi:")
            .ok_or("invalid WASAPI capture endpoint ID")?;
        let wide: Vec<u16> = raw.encode_utf16().chain(Some(0)).collect();
        // SAFETY: initialized MTA, explicit NUL-terminated endpoint ID; no default endpoint is used.
        let (device, manager) = unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
            let device = enumerator.GetDevice(PCWSTR(wide.as_ptr())).map_err(|e| e.to_string())?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
            (device, manager)
        };
        let (tx, rx) = mpsc::sync_channel(128);
        let failed = Arc::new(AtomicBool::new(false));
        let epoch = Arc::new(AtomicU64::new(1));
        let created: IAudioSessionNotification = Created {
            epoch: epoch.clone(),
            wake: wake.clone(),
            tx,
            failed: failed.clone(),
        }
        .into();
        // SAFETY: register before enumerating existing sessions. GetCount enables notifications.
        unsafe { manager.RegisterSessionNotification(&created) }.map_err(|e| e.to_string())?;
        let mut monitor = Self {
            wake,
            manager,
            created,
            device,
            sessions: BTreeMap::new(),
            rx,
            failed,
            epoch,
            _apartment: apartment,
        };
        // SAFETY: live manager in this MTA; count is bounded before indexing.
        unsafe {
            let existing = monitor.manager.GetSessionEnumerator().map_err(|e| e.to_string())?;
            let count = existing.GetCount().map_err(|e| e.to_string())?;
            if !(0..=128).contains(&count) {
                return Err("too many microphone capture sessions".into());
            }
            for index in 0..count {
                monitor.add(existing.GetSession(index).map_err(|e| e.to_string())?)?;
            }
        }
        Ok(monitor)
    }
    fn add(&mut self, control: IAudioSessionControl) -> Result<(), String> {
        let details: IAudioSessionControl2 = control.cast().map_err(|e| e.to_string())?;
        // SAFETY: live control in this MTA. Copy then release the owned COM string.
        let key = unsafe {
            if details.GetProcessId().map_err(|e| e.to_string())? == std::process::id() {
                return Ok(());
            }
            let raw = details.GetSessionInstanceIdentifier().map_err(|e| e.to_string())?;
            let value = raw.to_string();
            CoTaskMemFree(Some(raw.0.cast()));
            value.map_err(|e| e.to_string())?
        };
        if self.sessions.contains_key(&key) {
            return Ok(());
        }
        if self.sessions.len() >= 128 {
            return Err("too many microphone capture sessions".into());
        }
        let events: IAudioSessionEvents = Events {
            wake: self.wake.clone(),
            epoch: self.epoch.clone(),
        }
        .into();
        // SAFETY: register before querying; the Watch retains both references.
        unsafe { control.RegisterAudioSessionNotification(&events) }.map_err(|e| e.to_string())?;
        self.sessions.insert(key, Watch { control, events });
        self.epoch.fetch_add(1, Ordering::AcqRel);
        (self.wake)();
        Ok(())
    }
    fn observe(&mut self) -> Result<bool, String> {
        if self.failed.load(Ordering::Acquire) {
            return Err("microphone session notifications were lost".into());
        }
        // SAFETY: this exact recording endpoint must still be active.
        if unsafe { self.device.GetState() }.map_err(|e| e.to_string())? != DEVICE_STATE_ACTIVE {
            return Err("VB-CABLE recording endpoint disconnected".into());
        }
        for _ in 0..128 {
            let Ok(reference) = self.rx.try_recv() else {
                break;
            };
            self.add(reference.resolve().map_err(|e| e.to_string())?)?;
        }
        let mut active = false;
        let mut expired = Vec::new();
        for (key, watch) in &self.sessions {
            // SAFETY: recheck every retained session; notification loss is never interpreted as demand.
            let state = unsafe { watch.control.GetState() }.map_err(|e| e.to_string())?;
            active |= state == AudioSessionStateActive;
            if state == AudioSessionStateExpired {
                expired.push(key.clone());
            }
        }
        for key in expired {
            self.sessions.remove(&key);
        }
        Ok(active)
    }
    pub fn sample(&mut self) -> Demand {
        let sample = self.observe();
        match sample {
            Ok(active) => Demand {
                active,
                epoch: self.epoch.load(Ordering::Acquire),
                error: None,
            },
            Err(error) => Demand {
                active: false,
                epoch: self.epoch.load(Ordering::Acquire),
                error: Some(error),
            },
        }
    }
}
impl Drop for DemandMonitor {
    fn drop(&mut self) {
        // SAFETY: stop notifications before dropping sessions/interfaces/MTA.
        if let Err(error) = unsafe { self.manager.UnregisterSessionNotification(&self.created) } {
            tracing::warn!(%error, "microphone capture-session discovery cleanup failed");
        }
    }
}

pub(super) fn switch_default_input(expected: &str, replacement: &str) -> Result<(), String> {
    super::windows_default::switch(expected, replacement)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_endpoint_name_survives_cpal_device_description() {
        let description = cpal::DeviceDescriptionBuilder::new("CABLE Output")
            .driver("VB-Audio Virtual Cable")
            .add_extended_line("CABLE Output (VB-Audio Virtual Cable)")
            .build();
        assert_eq!(
            crate::device_name(&description),
            "CABLE Output (VB-Audio Virtual Cable)"
        );
        let no_extended = cpal::DeviceDescriptionBuilder::new("Headphones").build();
        assert_eq!(crate::device_name(&no_extended), "Headphones");
    }

    #[test]
    #[ignore = "read-only native enumeration; requires an installed standard VB-CABLE pair"]
    fn installed_vb_cable_is_discovered_without_opening_audio() {
        let _worker = WorkerApartment::new().unwrap();
        let output = super::super::virtual_output().unwrap();
        assert!(
            matches!(output, super::super::VirtualOutput::Ready { .. }),
            "{output:?}"
        );
        let mut monitor = DemandMonitor::new().unwrap();
        let demand = monitor.sample();
        assert!(demand.error.is_none(), "{demand:?}");
        assert!(
            super::super::inputs()
                .unwrap()
                .iter()
                .any(|device| device.name == "CABLE Output (VB-Audio Virtual Cable)")
        );
    }
}
