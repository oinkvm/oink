//! 枚举显示器：EnumDisplayMonitors / GetMonitorInfoW 给出物理像素矩形与主屏标记，
//! QueryDisplayConfig 给出目标设备路径（稳定 key）、友好名、输出技术、旋转，EDID 给出物理尺寸。

use oink_foundation::display::DisplayInfo;
use oink_foundation::geom::{Rect, Size};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DICS_FLAG_GLOBAL, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, DIREG_DEV, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, SetupDiOpenDevRegKey, SetupDiOpenDeviceInterfaceW,
};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_OTHER,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_ROTATION_ROTATE90,
    DISPLAYCONFIG_ROTATION_ROTATE180, DISPLAYCONFIG_ROTATION_ROTATE270, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo, GUID_DEVINTERFACE_MONITOR,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_BINARY, RegCloseKey, RegGetValueW, RegQueryValueExW,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::{BOOL, PCWSTR};

use super::dpi::ThreadDpiGuard;
use super::edid;
use crate::DisplayError;

/// GDI 层面的一块显示器（一个桌面区域；镜像组只有一个）。
#[derive(Debug)]
struct Monitor {
    rect: RECT,
    primary: bool,
    /// `\\.\DISPLAY1`
    gdi_name: String,
}

/// QueryDisplayConfig 的一条活动路径（源 → 目标显示器）。
#[derive(Debug, Default)]
pub(crate) struct Target {
    pub gdi_name: String,
    pub device_path: String,
    pub friendly_name: String,
    pub adapter_path: String,
    pub output_technology: i32,
    pub rotation_deg: u16,
}

pub(crate) fn displays() -> Result<Vec<DisplayInfo>, DisplayError> {
    let _dpi = ThreadDpiGuard::per_monitor_v2();
    let monitors = monitors()?;
    let targets = targets().unwrap_or_else(|e| {
        tracing::warn!("QueryDisplayConfig 失败，只使用 GDI 信息：{e}");
        Vec::new()
    });
    let mut out = Vec::with_capacity(monitors.len());
    for (i, m) in monitors.iter().enumerate() {
        // 镜像组：同一个源有多个目标，优先取非虚拟、有设备路径的那个
        let mut candidates: Vec<&Target> = targets.iter().filter(|t| t.gdi_name == m.gdi_name).collect();
        candidates.sort_by_key(|t| (is_virtual(t), t.device_path.is_empty()));
        let target = candidates.first().copied();
        out.push(build(i, m, target));
    }
    Ok(out)
}

fn build(index: usize, m: &Monitor, target: Option<&Target>) -> DisplayInfo {
    let w = (m.rect.right - m.rect.left).max(0);
    let h = (m.rect.bottom - m.rect.top).max(0);
    let rotation_deg = target.map_or(0, |t| t.rotation_deg);
    let edid = target
        .filter(|t| !t.device_path.is_empty())
        .and_then(|t| read_edid(&t.device_path))
        .and_then(|bytes| edid::parse(&bytes));
    let reference_extent = edid
        .as_ref()
        .and_then(|e| e.size_meters)
        .map(|s| edid::rotate(s, rotation_deg))
        .map(|(w, h)| {
            Size::new(
                w / oink_foundation::display::REFERENCE_DIAGONAL_METERS,
                h / oink_foundation::display::REFERENCE_DIAGONAL_METERS,
            )
        });
    let name = target
        .map(|t| t.friendly_name.clone())
        .filter(|n| !n.is_empty())
        .or_else(|| edid.as_ref().and_then(|e| e.name.clone()))
        .unwrap_or_else(|| format!("Display {}", index + 1));
    let key = target
        .map(|t| t.device_path.clone())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| m.gdi_name.clone());
    let info = DisplayInfo {
        key,
        name,
        bounds: Rect::new(
            f64::from(m.rect.left),
            f64::from(m.rect.top),
            f64::from(w),
            f64::from(h),
        ),
        pixel_width: w as u32,
        pixel_height: h as u32,
        reference_extent,
        rotation_deg,
        is_primary: m.primary,
        is_builtin: target.is_some_and(is_builtin),
        is_virtual: target.is_some_and(is_virtual),
    };
    tracing::debug!(
        gdi = %m.gdi_name,
        output_technology = target.map(|t| t.output_technology),
        adapter = target.map(|t| t.adapter_path.as_str()),
        edid_manufacturer = edid.as_ref().map(|e| e.manufacturer.as_str()),
        edid_product = edid.as_ref().map(|e| e.product),
        edid_name = edid.as_ref().and_then(|e| e.name.as_deref()),
        edid_size_meters = ?edid.as_ref().and_then(|e| e.size_meters),
        edid_basic_meters = ?edid.as_ref().and_then(|e| e.basic_meters),
        edid_dtd_meters = ?edid.as_ref().and_then(|e| e.dtd_meters),
        "display {}: {:?}",
        index,
        info
    );
    info
}

pub(crate) fn is_builtin(t: &Target) -> bool {
    let tech = t.output_technology;
    tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL.0
        || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED.0
        || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED.0
        || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS.0
}

/// 已知的虚拟显示器驱动（远程桌面、串流、虚拟屏软件）在设备路径或适配器路径里出现的标识。
const VIRTUAL_MARKERS: &[&str] = &[
    "ORAY",      // 向日葵 OrayIddDriver
    "IDDSAMPLE", // 微软 IddSampleDriver 及其衍生
    "PARSEC",    // Parsec Virtual Display
    "SPACEDESK", // spacedesk
    "USBMMIDD",  // usbmmidd 虚拟屏
    "MTTVDD",    // Virtual Display Driver (MikeTheTech)
    "VIRTUALDISPLAY",
    "VIRTUAL_DISPLAY",
    "SUNSHINE", // Sunshine / Apollo 虚拟屏
    "DEVDISP",  // Duet / 其他
];

pub(crate) fn is_virtual(t: &Target) -> bool {
    let tech = t.output_technology;
    if tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL.0 {
        return true;
    }
    let adapter = t.adapter_path.to_ascii_uppercase();
    let device = t.device_path.to_ascii_uppercase();
    if VIRTUAL_MARKERS
        .iter()
        .any(|m| adapter.contains(m) || device.contains(m))
    {
        return true;
    }
    // 由软件枚举（ROOT\、SWD\）的适配器上的间接显示器：不是真实的 GPU 或 USB 显示适配器
    let soft_adapter = adapter.starts_with(r"\\?\ROOT#") || adapter.starts_with(r"\\?\SWD#");
    soft_adapter
        && (tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED.0 || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_OTHER.0)
}

fn monitors() -> Result<Vec<Monitor>, DisplayError> {
    unsafe extern "system" fn cb(hmon: HMONITOR, _hdc: HDC, _rect: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` 是下面传入的 `&mut Vec<HMONITOR>`，只在 EnumDisplayMonitors 调用期间使用。
        let list = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        list.push(hmon);
        BOOL(1)
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    // SAFETY: 回调只在本调用期间执行，`handles` 在此期间有效。
    let ok = unsafe { EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut handles as *mut _ as isize)) };
    if !ok.as_bool() {
        return Err(DisplayError::Platform("EnumDisplayMonitors 失败".into()));
    }
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `mi` 是 MONITORINFOEXW，cbSize 已按其大小设置。
        let ok = unsafe { GetMonitorInfoW(h, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) };
        if !ok.as_bool() {
            continue;
        }
        out.push(Monitor {
            rect: mi.monitorInfo.rcMonitor,
            primary: mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            gdi_name: from_wide(&mi.szDevice),
        });
    }
    Ok(out)
}

pub(crate) fn targets() -> Result<Vec<Target>, DisplayError> {
    let mut paths: Vec<DISPLAYCONFIG_PATH_INFO>;
    let mut modes: Vec<DISPLAYCONFIG_MODE_INFO>;
    loop {
        let (mut np, mut nm) = (0u32, 0u32);
        // SAFETY: 输出参数指向本地变量。
        let rc = unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) };
        if rc != ERROR_SUCCESS {
            return Err(DisplayError::Platform(format!("GetDisplayConfigBufferSizes: {rc:?}")));
        }
        paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        // SAFETY: 缓冲区大小与传入的元素个数一致。
        let rc = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut np,
                paths.as_mut_ptr(),
                &mut nm,
                modes.as_mut_ptr(),
                None,
            )
        };
        if rc == ERROR_INSUFFICIENT_BUFFER {
            continue; // 两次调用之间配置变了
        }
        if rc != ERROR_SUCCESS {
            return Err(DisplayError::Platform(format!("QueryDisplayConfig: {rc:?}")));
        }
        paths.truncate(np as usize);
        break;
    }
    let mut out = Vec::with_capacity(paths.len());
    for p in &paths {
        let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        src.header = header(
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size_of_val(&src),
            p.sourceInfo.adapterId,
            p.sourceInfo.id,
        );
        let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        tgt.header = header(
            DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size_of_val(&tgt),
            p.targetInfo.adapterId,
            p.targetInfo.id,
        );
        let mut adp = DISPLAYCONFIG_ADAPTER_NAME::default();
        adp.header = header(
            DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
            size_of_val(&adp),
            p.targetInfo.adapterId,
            0,
        );
        // SAFETY: 每个请求包的 header.size 与其实际结构大小一致。
        let (rs, rt, ra) = unsafe {
            (
                DisplayConfigGetDeviceInfo(&mut src.header),
                DisplayConfigGetDeviceInfo(&mut tgt.header),
                DisplayConfigGetDeviceInfo(&mut adp.header),
            )
        };
        if rs != 0 {
            continue;
        }
        let rotation_deg = match p.targetInfo.rotation {
            r if r == DISPLAYCONFIG_ROTATION_ROTATE90 => 90,
            r if r == DISPLAYCONFIG_ROTATION_ROTATE180 => 180,
            r if r == DISPLAYCONFIG_ROTATION_ROTATE270 => 270,
            _ => 0,
        };
        out.push(Target {
            gdi_name: from_wide(&src.viewGdiDeviceName),
            device_path: if rt == 0 {
                from_wide(&tgt.monitorDevicePath)
            } else {
                String::new()
            },
            friendly_name: if rt == 0 {
                from_wide(&tgt.monitorFriendlyDeviceName)
            } else {
                String::new()
            },
            adapter_path: if ra == 0 {
                from_wide(&adp.adapterDevicePath)
            } else {
                String::new()
            },
            output_technology: p.targetInfo.outputTechnology.0,
            rotation_deg,
        });
    }
    Ok(out)
}

fn header(
    kind: windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_TYPE,
    size: usize,
    adapter: windows::Win32::Foundation::LUID,
    id: u32,
) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: size as u32,
        adapterId: adapter,
        id,
    }
}

/// 读取显示器的 EDID：先走 SetupAPI（设备接口 → 设备 → `Device Parameters` 键），失败时直接按设备实例路径读注册表。
pub(crate) fn read_edid(device_path: &str) -> Option<Vec<u8>> {
    if let Some(e) = read_edid_setupapi(device_path) {
        tracing::debug!("EDID（SetupAPI，{} 字节）：{device_path}", e.len());
        return Some(e);
    }
    let e = read_edid_registry(device_path);
    tracing::debug!("EDID（注册表回退，{:?} 字节）：{device_path}", e.as_ref().map(Vec::len));
    e
}

struct DevInfoList(HDEVINFO);
impl Drop for DevInfoList {
    fn drop(&mut self) {
        // SAFETY: 句柄由 SetupDiGetClassDevsW 返回，只销毁一次。
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

struct RegKey(HKEY);
impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: 键由 SetupDiOpenDevRegKey 打开，只关闭一次。
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn read_edid_setupapi(device_path: &str) -> Option<Vec<u8>> {
    let path = wide(device_path);
    // SAFETY: 所有指针参数指向本函数内有效的局部变量；缓冲区大小按 required 分配。
    unsafe {
        let set = DevInfoList(
            SetupDiGetClassDevsW(
                Some(&GUID_DEVINTERFACE_MONITOR),
                PCWSTR::null(),
                None,
                DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
            )
            .ok()?,
        );
        let mut ifd = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        SetupDiOpenDeviceInterfaceW(set.0, PCWSTR(path.as_ptr()), 0, Some(&mut ifd)).ok()?;
        let mut required = 0u32;
        let _ = SetupDiGetDeviceInterfaceDetailW(set.0, &ifd, None, 0, Some(&mut required), None);
        if required == 0 {
            return None;
        }
        let mut buf = vec![0u64; (required as usize).div_ceil(8)];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        let mut dev = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        SetupDiGetDeviceInterfaceDetailW(set.0, &ifd, Some(detail), required, None, Some(&mut dev)).ok()?;
        let key = RegKey(SetupDiOpenDevRegKey(set.0, &dev, DICS_FLAG_GLOBAL.0, 0, DIREG_DEV, KEY_READ.0).ok()?);
        let name = wide("EDID");
        let mut len = 0u32;
        if RegQueryValueExW(key.0, PCWSTR(name.as_ptr()), None, None, None, Some(&mut len)) != ERROR_SUCCESS || len == 0
        {
            return None;
        }
        let mut data = vec![0u8; len as usize];
        if RegQueryValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            None,
            Some(data.as_mut_ptr()),
            Some(&mut len),
        ) != ERROR_SUCCESS
        {
            return None;
        }
        data.truncate(len as usize);
        Some(data)
    }
}

/// `\\?\DISPLAY#GSM5B09#5&2b3c&0&UID4352#{e6f07b5f-...}` → `DISPLAY\GSM5B09\5&2b3c&0&UID4352`
pub(crate) fn instance_id_from_interface_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\").unwrap_or(path);
    let end = rest.rfind("#{").unwrap_or(rest.len());
    let id = rest[..end].replace('#', r"\");
    (!id.is_empty()).then_some(id)
}

fn read_edid_registry(device_path: &str) -> Option<Vec<u8>> {
    let id = instance_id_from_interface_path(device_path)?;
    let sub = wide(&format!(r"SYSTEM\CurrentControlSet\Enum\{id}\Device Parameters"));
    let name = wide("EDID");
    let mut len = 0u32;
    // SAFETY: 字符串以 NUL 结尾；第一次只取长度，第二次缓冲区按长度分配。
    unsafe {
        let rc = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            None,
            Some(&mut len),
        );
        if rc != ERROR_SUCCESS || len == 0 {
            return None;
        }
        let mut data = vec![0u8; len as usize];
        let rc = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut len),
        );
        if rc != ERROR_SUCCESS {
            return None;
        }
        data.truncate(len as usize);
        Some(data)
    }
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(tech: i32, adapter: &str, device: &str) -> Target {
        Target {
            output_technology: tech,
            adapter_path: adapter.into(),
            device_path: device.into(),
            ..Default::default()
        }
    }

    #[test]
    fn instance_id() {
        assert_eq!(
            instance_id_from_interface_path(
                r"\\?\DISPLAY#GSM5B09#5&2b3c1a&0&UID4352#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"
            )
            .as_deref(),
            Some(r"DISPLAY\GSM5B09\5&2b3c1a&0&UID4352")
        );
    }

    #[test]
    fn classifies_builtin_and_virtual() {
        let pci = r"\\?\PCI#VEN_10DE&DEV_2684#4&1#{5b45201d-f2f2-4f3b-85bb-30ff1f953599}";
        assert!(is_builtin(&target(DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL.0, pci, "")));
        assert!(is_builtin(&target(
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED.0,
            pci,
            ""
        )));
        assert!(!is_builtin(&target(5, pci, "")));
        assert!(!is_virtual(&target(5, pci, r"\\?\DISPLAY#GSM5B09#x#{g}")));
        assert!(is_virtual(&target(
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL.0,
            pci,
            ""
        )));
        assert!(is_virtual(&target(
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED.0,
            r"\\?\ROOT#DISPLAY#0000#{5b45201d}",
            ""
        )));
        assert!(is_virtual(&target(
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED.0,
            r"\\?\USB#VID_17E9#x#{g}",
            r"\\?\DISPLAY#ORAY001#x#{g}"
        )));
        // DisplayLink 等 USB 显示适配器：真实显示器
        assert!(!is_virtual(&target(
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED.0,
            r"\\?\USB#VID_17E9&PID_4301#x#{g}",
            r"\\?\DISPLAY#DEL4123#x#{g}"
        )));
    }
}
