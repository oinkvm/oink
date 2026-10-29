//! Called only on the SYSTEM helper's ordinary query worker.
use super::*;
use crate::power::{Layout, Summary};
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtPowerInformation(
        level: u32,
        input: *const std::ffi::c_void,
        input_len: u32,
        output: *mut std::ffi::c_void,
        output_len: u32,
    ) -> i32;
    fn RtlGetVersion(version: *mut Version) -> i32;
}
#[repr(C)]
struct Version {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    service_pack: [u16; 128],
}
pub(super) fn observe() -> Result<Summary, &'static str> {
    if cfg!(not(target_pointer_width = "64")) {
        return Err("unsupported pointer width");
    }
    let mut version = Version {
        size: std::mem::size_of::<Version>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // SAFETY: sized, initialized version structure lives through the call.
    if unsafe { RtlGetVersion(&mut version) } < 0 {
        return Err("version unavailable");
    }
    let layout = Layout::windows(version.major, version.build).ok_or("unsupported Windows version")?;
    let mut size = 16384usize;
    loop {
        let mut bytes = vec![0u8; size];
        // SAFETY: bounded writable buffer and no input; the private ABI is parsed
        // as checked bytes, never cast to dereferenceable kernel pointers.
        let status = unsafe { NtPowerInformation(45, ptr::null(), 0, bytes.as_mut_ptr().cast(), size as u32) };
        if status >= 0 {
            return crate::power::parse(&bytes, layout, exclude);
        }
        if !matches!(status as u32, 0xc0000023 | 0xc0000004 | 0x80000005) || size >= 1024 * 1024 {
            return Err("power request list unavailable");
        }
        size *= 2;
    }
}
fn exclude(pid: u32) -> bool {
    // SAFETY: read-only process handle pins PID identity during image resolution.
    let Ok(process) = Handle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) }) else {
        return true;
    };
    let Ok(path) = security::image(process.0) else {
        return true;
    };
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    matches!(name.as_str(), "oink.exe" | "oink-agent.exe" | "oink-service.exe")
}
