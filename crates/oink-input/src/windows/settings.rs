//! 读取本机手感设置：按键重复、滚轮每格行数、鼠标与触摸板的滚动方向。

use oink_foundation::input::FeelProfile;
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Input::{
    GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RIDI_DEVICENAME, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETKEYBOARDDELAY, SPI_GETKEYBOARDSPEED, SPI_GETWHEELSCROLLLINES, SYSTEM_PARAMETERS_INFO_ACTION,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};
use windows::core::PCWSTR;

/// `SPI_GETWHEELSCROLLLINES` 返回这个值表示「一次滚动一屏」。
const WHEEL_PAGESCROLL: u32 = u32::MAX;
/// 「一次滚动一屏」模式下按多少行换算（没有准确的行数，取系统默认值）。
pub(crate) const PAGE_SCROLL_EQUIV_LINES: f64 = 3.0;
/// `SPI_GETTOUCHPADPARAMETERS`（Windows 11 22H2 起；windows crate 尚未收录）。
const SPI_GETTOUCHPADPARAMETERS: SYSTEM_PARAMETERS_INFO_ACTION = SYSTEM_PARAMETERS_INFO_ACTION(0x00AE);

fn spi_u32(action: SYSTEM_PARAMETERS_INFO_ACTION) -> Option<u32> {
    let mut v = 0u32;
    // SAFETY: 这些 SPI 查询项的 pvParam 都是一个 UINT。
    unsafe {
        SystemParametersInfoW(
            action,
            0,
            Some(&mut v as *mut u32 as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .ok()?;
    Some(v)
}

/// 滚轮每格行数（「一屏」模式按 [`PAGE_SCROLL_EQUIV_LINES`]；读取失败按 3）。0 表示用户关闭了滚轮滚动。
pub(crate) fn wheel_lines_per_notch() -> f64 {
    match spi_u32(SPI_GETWHEELSCROLLLINES) {
        Some(WHEEL_PAGESCROLL) => PAGE_SCROLL_EQUIV_LINES,
        Some(n) => f64::from(n),
        None => 3.0,
    }
}

/// `SPI_GETKEYBOARDDELAY`：0..=3 → 250..=1000 ms。
pub(crate) fn repeat_delay_ms(v: u32) -> u32 {
    (v.min(3) + 1) * 250
}

/// `SPI_GETKEYBOARDSPEED`：0..=31 → 约 2.5..=30 次/秒（线性近似），返回间隔毫秒。
pub(crate) fn repeat_interval_ms(v: u32) -> u32 {
    let hz = 2.5 + f64::from(v.min(31)) * (30.0 - 2.5) / 31.0;
    (1000.0 / hz).round() as u32
}

pub(crate) fn feel_profile() -> FeelProfile {
    FeelProfile {
        key_repeat_delay_ms: repeat_delay_ms(spi_u32(SPI_GETKEYBOARDDELAY).unwrap_or(1)),
        key_repeat_interval_ms: repeat_interval_ms(spi_u32(SPI_GETKEYBOARDSPEED).unwrap_or(31)),
        wheel_lines_per_notch: Some(wheel_lines_per_notch()),
        natural_scroll_mouse: mouse_flip_flop_wheel().iter().any(|(_, flip)| *flip),
        natural_scroll_touchpad: touchpad_natural_scroll().unwrap_or(true),
    }
}

/// `TOUCHPAD_PARAMETERS`（版本 1）。位域按 MSVC 布局展开成两个 `u32`。
#[repr(C)]
#[derive(Default)]
struct TouchpadParameters {
    version_number: u32,
    max_supported_contacts: u32,
    legacy_touchpad_features: u32,
    flags1: u32,
    flags2: u32,
    sensitivity_level: u32,
    cursor_speed: u32,
    feedback_intensity: u32,
    click_force_sensitivity: u32,
    right_click_zone_width: u32,
    right_click_zone_height: u32,
}

/// 精确式触摸板的滚动方向是否为「自然」（手指向下 → 内容向下，即「向下滑动时向上滚动」，Windows 的默认值）。
///
/// 先用 `SPI_GETTOUCHPADPARAMETERS.scrollDirectionReversed`（为真表示改成了「向下滑动时向下滚动」），
/// 系统不支持时读注册表 `HKCU\...\PrecisionTouchPad\ScrollDirection`（0 = 向下滑动时向上滚动）。
/// 两个来源的语义都来自文档与社区资料，尚待真实 Windows 触摸板验证。
pub(crate) fn touchpad_natural_scroll() -> Option<bool> {
    let mut p = TouchpadParameters {
        version_number: 1,
        ..Default::default()
    };
    // SAFETY: 传入结构体大小与指针，结构体按文档布局。
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETTOUCHPADPARAMETERS,
            size_of::<TouchpadParameters>() as u32,
            Some(&mut p as *mut TouchpadParameters as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok();
    if ok {
        let touchpad_present = p.flags1 & 1 != 0;
        let reversed = p.flags2 & (1 << 9) != 0;
        tracing::debug!(touchpad_present, reversed, "SPI_GETTOUCHPADPARAMETERS");
        return Some(!reversed);
    }
    let v = reg_dword(
        HKEY_CURRENT_USER,
        r"Software\Microsoft\Windows\CurrentVersion\PrecisionTouchPad",
        "ScrollDirection",
    )?;
    Some(v == 0)
}

pub(crate) fn reg_dword(root: HKEY, sub: &str, name: &str) -> Option<u32> {
    let sub: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
    let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut v = 0u32;
    let mut len = 4u32;
    // SAFETY: 字符串以 NUL 结尾；输出缓冲区为 4 字节 DWORD。
    let rc = unsafe {
        RegGetValueW(
            root,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut _),
            Some(&mut len),
        )
    };
    (rc == ERROR_SUCCESS).then_some(v)
}

/// `\\?\HID#VID_046D&PID_C52B&MI_01&Col01#8&2a6b&0&0000#{378de44c-...}` → `HID\VID_046D&PID_C52B&MI_01&Col01\8&2a6b&0&0000`
pub(crate) fn instance_id_from_interface_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\").unwrap_or(path);
    let end = rest.rfind("#{").unwrap_or(rest.len());
    let id = rest[..end].replace('#', r"\");
    (!id.is_empty()).then_some(id)
}

/// 当前连接的鼠标（Raw Input 设备列表）及其 `FlipFlopWheel` 设置（设备实例的 `Device Parameters` 下，
/// 1 表示翻转滚轮方向，由 mouhid 驱动在事件到达钩子之前应用）。
pub(crate) fn mouse_flip_flop_wheel() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for (handle, name) in raw_mice() {
        let _ = handle;
        let flip = instance_id_from_interface_path(&name)
            .and_then(|id| {
                reg_dword(
                    HKEY_LOCAL_MACHINE,
                    &format!(r"SYSTEM\CurrentControlSet\Enum\{id}\Device Parameters"),
                    "FlipFlopWheel",
                )
            })
            .unwrap_or(0)
            == 1;
        out.push((name, flip));
    }
    out
}

fn raw_mice() -> Vec<(HANDLE, String)> {
    let mut n = 0u32;
    let cb = size_of::<RAWINPUTDEVICELIST>() as u32;
    // SAFETY: 第一次只取数量，第二次缓冲区按数量分配。
    unsafe {
        if GetRawInputDeviceList(None, &mut n, cb) == u32::MAX || n == 0 {
            return Vec::new();
        }
        let mut list = vec![RAWINPUTDEVICELIST::default(); n as usize];
        let got = GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut n, cb);
        if got == u32::MAX {
            return Vec::new();
        }
        list.truncate(got as usize);
        list.into_iter()
            .filter(|d| d.dwType == RIM_TYPEMOUSE)
            .filter_map(|d| device_name(d.hDevice).map(|n| (d.hDevice, n)))
            .collect()
    }
}

pub(crate) fn device_name(h: HANDLE) -> Option<String> {
    let mut len = 0u32;
    // SAFETY: 第一次取字符数，第二次按字符数分配 UTF-16 缓冲区。
    unsafe {
        GetRawInputDeviceInfoW(Some(h), RIDI_DEVICENAME, None, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        let r = GetRawInputDeviceInfoW(Some(h), RIDI_DEVICENAME, Some(buf.as_mut_ptr().cast()), &mut len);
        if r == u32::MAX {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn repeat_conversions() {
        assert_eq!(repeat_delay_ms(0), 250);
        assert_eq!(repeat_delay_ms(1), 500);
        assert_eq!(repeat_delay_ms(3), 1000);
        assert_eq!(repeat_delay_ms(9), 1000);
        assert_eq!(repeat_interval_ms(31), 33);
        assert_eq!(repeat_interval_ms(0), 400);
        assert!(repeat_interval_ms(15) > 33 && repeat_interval_ms(15) < 400);
    }

    #[test]
    fn instance_id() {
        assert_eq!(
            instance_id_from_interface_path(
                r"\\?\HID#VID_046D&PID_C52B&MI_01&Col01#8&2a6b6b0e&0&0000#{378de44c-56ef-11d1-bc8c-00a0c91405dd}"
            )
            .as_deref(),
            Some(r"HID\VID_046D&PID_C52B&MI_01&Col01\8&2a6b6b0e&0&0000")
        );
    }

    #[test]
    fn touchpad_parameters_size() {
        assert_eq!(size_of::<TouchpadParameters>(), 44);
    }
}
