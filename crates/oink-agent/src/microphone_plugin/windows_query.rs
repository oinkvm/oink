//! Read-only SetupAPI/registry inspection. No PowerShell, WMI or COM apartment.
use super::Action;
use ::windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::*,
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_INVALID_DATA, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND},
        System::Registry::*,
    },
    core::PCWSTR,
};

const RESTART_MARKER: &str = r"SOFTWARE\Oink\MicrophoneRestartRequired";
const ARCHIVE: &str = "b950e39f01af1d04ea623c8f6d8eb9b6ea5c477c637295fabf20631c85116bfb";
const MARKER: &str = r"SOFTWARE\Oink\Microphone";
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn text(data: &[u16]) -> String {
    String::from_utf16_lossy(data.split(|c| *c == 0).next().unwrap_or_default())
}
fn value(path: &str, name: &str) -> Result<Option<String>, String> {
    let path = wide(path);
    let name = wide(name);
    let mut data = vec![0u16; 32768];
    let mut size = (data.len() * 2) as u32;
    // SAFETY: NUL-terminated names and writable UTF-16 output; read-only 64-bit HKLM view.
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if result == ERROR_FILE_NOT_FOUND || result == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    result.ok().map_err(|e| e.to_string())?;
    Ok(Some(text(&data)))
}
fn exists(path: &str) -> Result<bool, String> {
    let path = wide(path);
    let mut key = HKEY::default();
    // SAFETY: valid string and output handle, read-only key access.
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path.as_ptr()),
            None,
            KEY_READ | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if result == ERROR_FILE_NOT_FOUND || result == ERROR_PATH_NOT_FOUND {
        return Ok(false);
    }
    result.ok().map_err(|e| e.to_string())?;
    // SAFETY: successful open above transferred a valid handle.
    unsafe {
        let _ = RegCloseKey(key);
    }
    Ok(true)
}
struct Devices(HDEVINFO);
impl Drop for Devices {
    fn drop(&mut self) {
        // SAFETY: the guard owns this device-information set and drops it once.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}
fn property(
    devices: HDEVINFO,
    device: &SP_DEVINFO_DATA,
    prop: SETUP_DI_REGISTRY_PROPERTY,
) -> Result<Vec<String>, String> {
    let mut data = vec![0u8; 65536];
    // SAFETY: live device set and writable buffer; properties are REG_SZ/REG_MULTI_SZ.
    if let Err(error) = unsafe { SetupDiGetDeviceRegistryPropertyW(devices, device, prop, None, Some(&mut data), None) }
    {
        if error.code() == ERROR_INVALID_DATA.to_hresult() {
            return Ok(Vec::new());
        }
        return Err(error.to_string());
    }
    let words: Vec<u16> = data.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
    Ok(words
        .split(|c| *c == 0)
        .take_while(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect())
}
#[derive(Debug)]
struct Driver {
    device: String,
    inf: String,
    version: String,
}
fn drivers() -> Result<Vec<Driver>, String> {
    // A retained driver package/service or non-present devnode is not an installed device.
    // DIGCF_PRESENT still includes disabled devices, preserving ownership protection.
    // SAFETY: null filters request the local device inventory, without modifying it.
    let devices = Devices(
        unsafe { SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_ALLCLASSES | DIGCF_PRESENT) }
            .map_err(|e| e.to_string())?,
    );
    let mut found = Vec::new();
    let started = std::time::Instant::now();
    for index in 0.. {
        if started.elapsed().as_secs() >= 10 {
            return Err("Microphone device enumeration timed out".into());
        }
        let mut device = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: valid set and properly sized writable device record.
        if let Err(error) = unsafe { SetupDiEnumDeviceInfo(devices.0, index, &mut device) } {
            if error.code() == ERROR_NO_MORE_ITEMS.to_hresult() {
                break;
            }
            return Err(error.to_string());
        }
        let mut id = [0u16; 4096];
        // SAFETY: live device record and writable output.
        unsafe { SetupDiGetDeviceInstanceIdW(devices.0, &device, Some(&mut id), None) }.map_err(|e| e.to_string())?;
        let id = text(&id);
        let hardware = property(devices.0, &device, SPDRP_HARDWAREID)?;
        if !id.to_ascii_uppercase().contains("VBAUDIOVACWDM")
            && !hardware.iter().any(|h| h.eq_ignore_ascii_case("VBAudioVACWDM"))
        {
            continue;
        }
        let key = property(devices.0, &device, SPDRP_DRIVER)?.into_iter().next();
        // An unconfigured devnode has no installed driver (same as signed-driver inventory).
        if let Some(key) = key {
            let path = format!(r"SYSTEM\CurrentControlSet\Control\Class\{key}");
            if let (Some(inf), Some(version)) = (value(&path, "InfPath")?, value(&path, "DriverVersion")?) {
                found.push(Driver {
                    device: id,
                    inf,
                    version,
                });
            }
        }
    }
    Ok(found)
}
fn matches(
    drivers: &[Driver],
    archive: Option<String>,
    device: Option<String>,
    inf: Option<String>,
    version: Option<String>,
) -> bool {
    let [driver] = drivers else {
        return false;
    };
    [
        (archive.as_deref(), ARCHIVE),
        (device.as_deref(), driver.device.as_str()),
        (inf.as_deref(), driver.inf.as_str()),
        (version.as_deref(), driver.version.as_str()),
    ]
    .iter()
    .all(|(a, b)| a.is_some_and(|a| a.eq_ignore_ascii_case(b)))
}
pub(super) fn query() -> Result<(bool, bool, bool, Option<Action>), String> {
    let drivers = drivers()?;
    let installed = !drivers.is_empty();
    let managed = matches(
        &drivers,
        value(MARKER, "Archive")?,
        value(MARKER, "Device")?,
        value(MARKER, "Inf")?,
        value(MARKER, "Version")?,
    );
    let restart_required = exists(RESTART_MARKER)?;
    let last_operation = last_operation(restart_required, value(RESTART_MARKER, "LastOperation")?.as_deref());
    Ok((installed, managed, restart_required, last_operation))
}
fn last_operation(restart_required: bool, value: Option<&str>) -> Option<Action> {
    match (restart_required, value) {
        (true, Some("install")) => Some(Action::Install),
        (true, Some("uninstall")) => Some(Action::Uninstall),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn last_operation_requires_live_marker_and_known_action() {
        assert_eq!(last_operation(true, Some("install")), Some(Action::Install));
        assert_eq!(last_operation(true, Some("uninstall")), Some(Action::Uninstall));
        assert_eq!(last_operation(false, Some("install")), None);
        assert_eq!(last_operation(false, None), None);
        assert_eq!(last_operation(true, None), None); // Older marker.
        assert_eq!(last_operation(true, Some("unknown")), None);
    }
    #[test]
    fn ownership_requires_exact_unique_driver_and_all_marker_fields() {
        let driver = || Driver {
            device: "ROOT\\MEDIA\\0000".into(),
            inf: "oem42.inf".into(),
            version: "1.0".into(),
        };
        let check = |ds: &[Driver], archive: Option<String>, inf: &str| {
            matches(
                ds,
                archive,
                Some("root\\media\\0000".into()),
                Some(inf.into()),
                Some("1.0".into()),
            )
        };
        assert!(check(&[driver()], Some(ARCHIVE.into()), "OEM42.INF"));
        assert!(!check(&[driver()], None, "oem42.inf"));
        assert!(!check(&[driver()], Some(ARCHIVE.into()), "oem41.inf"));
        assert!(!check(&[driver(), driver()], Some(ARCHIVE.into()), "oem42.inf"));
        assert!(!check(&[], Some(ARCHIVE.into()), "oem42.inf"));
    }
    #[test]
    #[ignore = "read-only real-machine timing probe"]
    fn native_query_timing() {
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let result = query();
            println!(
                "native_query_ms={} result={result:?}",
                start.elapsed().as_secs_f64() * 1000.
            );
            assert!(result.is_ok());
        }
        crate::microphone_plugin::start();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while crate::microphone_plugin::status().refreshing {
            assert!(std::time::Instant::now() < deadline, "startup query did not finish");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        println!("cached_status={:?}", crate::microphone_plugin::status());
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(crate::microphone_plugin::status());
        }
        println!(
            "cached_status_mean_us={} over=10000",
            start.elapsed().as_secs_f64() * 1_000_000. / 10_000.
        );
    }
}
