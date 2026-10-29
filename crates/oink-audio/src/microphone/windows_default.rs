//! Default capture endpoint mutation, only for an approved follow-default repair.
use super::windows::WorkerApartment;
use windows::{
    Win32::{
        Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole},
        System::Com::{CLSCTX_ALL, CoCreateInstance},
    },
    core::{GUID, HRESULT, IUnknown_Vtbl, Interface, PCWSTR},
};
windows::core::define_interface!(PolicyConfig, PolicyConfigVtbl, 0xf8679f50_850a_41cf_9c72_430f290290c8);
#[repr(C)]
pub struct PolicyConfigVtbl {
    base: IUnknown_Vtbl,
    // IPolicyConfig's ten format/period/share/property methods precede this slot.
    preceding: [usize; 10],
    set_default_endpoint: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, i32) -> HRESULT,
    set_endpoint_visibility: usize,
}
impl std::fmt::Debug for PolicyConfigVtbl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyConfigVtbl").finish_non_exhaustive()
    }
}
pub(super) fn switch(expected: &str, replacement: &str) -> Result<(), String> {
    let _apartment = WorkerApartment::new()?;
    // CPAL uses the console capture role. Changing only this role preserves
    // communications/multimedia and every playback choice. Windows' public
    // MMDevice API has no setter, so use the same IPolicyConfig ABI as our
    // installer default-restoration path (Windows 11); this new runtime setter
    // awaits a real-device test. Never call it for an explicit input selection.
    if super::default_input()?.as_ref().map(|i| i.id.as_str()) != Some(expected) {
        return Err("system default microphone changed before loopback repair".into());
    }
    let id = replacement
        .strip_prefix("wasapi:")
        .filter(|id| !id.contains('\0'))
        .ok_or("invalid WASAPI capture endpoint ID")?;
    let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
    // SAFETY: this worker owns the COM apartment, class/interface ABI is the
    // existing installer IPolicyConfig ABI, and the endpoint string stays live.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let device = enumerator.GetDevice(PCWSTR(wide.as_ptr())).map_err(|e| e.to_string())?;
        use windows::Win32::Media::Audio::{DEVICE_STATE_ACTIVE, IMMEndpoint};
        let endpoint: IMMEndpoint = device.cast().map_err(|e| e.to_string())?;
        if endpoint.GetDataFlow().map_err(|e| e.to_string())? != eCapture
            || device.GetState().map_err(|e| e.to_string())? != DEVICE_STATE_ACTIVE
        {
            return Err("replacement capture endpoint is unavailable".into());
        }
        let policy: PolicyConfig = CoCreateInstance(
            &GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9),
            None,
            CLSCTX_ALL,
        )
        .map_err(|e| e.to_string())?;
        (policy.vtable().set_default_endpoint)(policy.as_raw(), PCWSTR(wide.as_ptr()), eConsole.0)
            .ok()
            .map_err(|e| format!("Windows default microphone switch failed: {e}"))?;
    }
    if super::default_input()?.as_ref().map(|i| i.id.as_str()) != Some(replacement) {
        return Err("Windows default microphone switch was not confirmed".into());
    }
    crate::device_notifications::invalidate();
    Ok(())
}

/// Resolve the recording endpoint to its adapter and inspect the driver's
/// hardware identity. Endpoint display names are user-editable, so a renamed
/// VB-CABLE must still be excluded from microphone-source selection.
pub(super) fn is_forwarding_endpoint(id: &str) -> Result<bool, String> {
    use windows::Win32::{
        Devices::FunctionDiscovery::{PKEY_Device_HardwareIds, PKEY_Device_InstanceId},
        Media::Audio::IDeviceTopology,
        System::Com::{
            CoTaskMemFree, STGM_READ,
            StructuredStorage::{PropVariantGetElementCount, PropVariantGetStringElem, PropVariantToStringAlloc},
        },
    };
    let _apartment = WorkerApartment::new()?;
    let raw = id
        .strip_prefix("wasapi:")
        .filter(|s| !s.contains('\0'))
        .ok_or("invalid WASAPI endpoint ID")?;
    let wide: Vec<u16> = raw.encode_utf16().chain(Some(0)).collect();
    // SAFETY: read-only COM metadata on the owning apartment. Every allocated
    // endpoint/property string is copied and freed before returning.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let endpoint = enumerator.GetDevice(PCWSTR(wide.as_ptr())).map_err(|e| e.to_string())?;
        let topology: IDeviceTopology = endpoint.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
        let connector = topology.GetConnector(0).map_err(|e| e.to_string())?;
        let adapter_id = connector.GetDeviceIdConnectedTo().map_err(|e| e.to_string())?;
        let adapter = enumerator.GetDevice(PCWSTR(adapter_id.0));
        CoTaskMemFree(Some(adapter_id.0.cast()));
        let properties = adapter
            .map_err(|e| e.to_string())?
            .OpenPropertyStore(STGM_READ)
            .map_err(|e| e.to_string())?;
        if let Ok(hardware) = properties.GetValue(&PKEY_Device_HardwareIds) {
            for i in 0..PropVariantGetElementCount(&hardware) {
                let item = PropVariantGetStringElem(&hardware, i).map_err(|e| e.to_string())?;
                let identity = item.to_string();
                CoTaskMemFree(Some(item.0.cast()));
                if identity
                    .map_err(|e| e.to_string())?
                    .to_ascii_uppercase()
                    .contains("VBAUDIOVACWDM")
                {
                    return Ok(true);
                }
            }
        }
        // MMDevice's adapter store can omit HardwareIds (observed on Windows
        // 11 VB-CABLE), while exposing its PnP instance ID. Query that exact
        // existing devnode through SetupAPI; do not infer identity from a name.
        let instance = properties
            .GetValue(&PKEY_Device_InstanceId)
            .map_err(|e| e.to_string())?;
        let instance = PropVariantToStringAlloc(&instance).map_err(|e| e.to_string())?;
        let value = instance.to_string();
        CoTaskMemFree(Some(instance.0.cast()));
        adapter_is_forwarding_driver(&value.map_err(|e| e.to_string())?)
    }
}
fn adapter_is_forwarding_driver(instance: &str) -> Result<bool, String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::*;
    struct Devices(HDEVINFO);
    impl Drop for Devices {
        fn drop(&mut self) {
            // SAFETY: owns exactly this SetupAPI inventory handle.
            let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }
    let id: Vec<u16> = instance.encode_utf16().chain(Some(0)).collect();
    let mut device = SP_DEVINFO_DATA {
        cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };
    // SAFETY: read-only device inventory and exact terminated instance ID.
    let devices = Devices(unsafe { SetupDiCreateDeviceInfoList(None, None) }.map_err(|e| e.to_string())?);
    // SAFETY: valid handle, writable initialized device record and existing ID.
    unsafe { SetupDiOpenDeviceInfoW(devices.0, PCWSTR(id.as_ptr()), None, 0, Some(&mut device)) }
        .map_err(|e| e.to_string())?;
    let mut bytes = [0u8; 8192];
    // SAFETY: valid inventory record and writable hardware-ID MULTI_SZ buffer.
    unsafe { SetupDiGetDeviceRegistryPropertyW(devices.0, &device, SPDRP_HARDWAREID, None, Some(&mut bytes), None) }
        .map_err(|e| e.to_string())?;
    let values: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    Ok(values.split(|c| *c == 0).any(|id| {
        String::from_utf16_lossy(id)
            .to_ascii_uppercase()
            .contains("VBAUDIOVACWDM")
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "read-only native endpoint inventory; no input stream or default write"]
    fn forwarding_input_identity_matches_the_installed_vendor_endpoint() {
        let _apartment = super::WorkerApartment::new().unwrap();
        let candidates = super::super::input_candidates().unwrap();
        for candidate in &candidates {
            println!(
                "input={:?} physical={} built_in={} forwarding_loopback={}",
                candidate.input, candidate.physical, candidate.built_in, candidate.forwarding_loopback
            );
            if candidate.input.name.contains("VB-Audio Virtual Cable") {
                assert!(
                    candidate.forwarding_loopback,
                    "vendor endpoint was not recognized by hardware identity: {:?}",
                    super::is_forwarding_endpoint(&candidate.input.id)
                );
            }
        }
    }
}
