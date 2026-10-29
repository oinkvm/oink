//! Raw Input：区分精确式触摸板与鼠标，并为钩子看门狗提供「设备确实有输入」的旁证。
//!
//! 注册（都用 `RIDEV_INPUTSINK`，窗口不在前台也能收到）：
//! - 精确式触摸板的数字化仪集合（HID Usage Page 0x0D / Usage 0x05）：用 `HidP_GetUsageValue` 读报告里的
//!   Contact Count（0x0D/0x54）。最近 [`TOUCH_WINDOW`] 内有 ≥ 1 个触点的报告，就把同期的指针/滚轮事件标为
//!   `SourceKind::Touchpad`。
//! - 鼠标（0x01/0x02）与键盘（0x01/0x06）：分别记录时间，供看门狗分别判断鼠标、键盘低级钩子是否被系统静默卸载。
//!
//! **注册会被同一进程里的其他代码覆盖**：Raw Input 按进程、按用途（Usage Page/Usage）只有一个目标窗口，后注册
//! 的生效。界面程序（Tauri）的事件循环 tao 在 `EventLoop::new` 与 `set_device_event_filter` 里为鼠标、键盘注册
//! 它自己的窗口（`RIDEV_DEVNOTIFY`，默认过滤 `Unfocused`），而服务进程（含截获）在界面构建之前启动，于是截获线程
//! 的注册被覆盖、再也收不到鼠标和键盘的 `WM_INPUT`，看门狗从来没有生效（评审 W01）。所以看门狗每次先核对
//! 注册（[`registration_ok`]），被改走了就记一条警告并重新注册。tao 只把这些 Raw Input 用于 `DeviceEvent`，
//! 界面没有使用。
//!
//! 局限：
//! - 吞掉低级钩子的 MOVE 会影响 Raw Input，所以游戏模式保留 MOVE；不能推断所有 wheel 的顺序也一样。
//!   build 26200 实测 wheel 的 WM_INPUT 可在 hook 前或后分派，吞掉 wheel 后也能收到它。
//!   数字化仪报告不经过鼠标钩子，所以 Remote 模式下仍能判断触摸板。
//! - 数字化仪触点与鼠标回调仍按时间窗口关联，
//!   手指刚放上的第一个事件可能仍被标为鼠标；触摸板上有手指时同时移动外接鼠标，鼠标事件会被误标为触摸板。
//! - SendInput 的 mouse raw report 可以没有设备句柄，不能充当实体设备的 watchdog 证据。
//!   wheel 的精确对应关系另由 wheel_origin 同时处理已分派和待处理的报告。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::Devices::HumanInterfaceDevice::{
    HIDP_STATUS_SUCCESS, HidP_GetUsageValue, HidP_Input, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM};
use windows::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, GetRegisteredRawInputDevices, HRAWINPUT, RAWHID, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIDI_PREPARSEDDATA, RIM_TYPEHID, RIM_TYPEKEYBOARD,
    RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::GetMessageTime;

/// 触摸板「有手指」的判定窗口。
pub(crate) const TOUCH_WINDOW: Duration = Duration::from_millis(100);

const PAGE_DIGITIZER: u16 = 0x0D;
const USAGE_TOUCHPAD: u16 = 0x05;
const USAGE_CONTACT_COUNT: u16 = 0x54;

fn devices(hwnd: HWND, flags: windows::Win32::UI::Input::RAWINPUTDEVICE_FLAGS) -> [RAWINPUTDEVICE; 3] {
    let d = |page: u16, usage: u16| RAWINPUTDEVICE {
        usUsagePage: page,
        usUsage: usage,
        dwFlags: flags,
        hwndTarget: hwnd,
    };
    [d(0x01, 0x02), d(0x01, 0x06), d(PAGE_DIGITIZER, USAGE_TOUCHPAD)]
}

pub(crate) fn register(hwnd: HWND) -> Result<(), String> {
    let devs = devices(hwnd, RIDEV_INPUTSINK | windows::Win32::UI::Input::RIDEV_DEVNOTIFY);
    // SAFETY: 设备数组在调用期间有效，cbSize 为单个结构大小。
    unsafe { RegisterRawInputDevices(&devs, size_of::<RAWINPUTDEVICE>() as u32) }.map_err(|e| e.to_string())
}

/// 本进程当前的 Raw Input 注册里，鼠标、键盘、触摸板三项是否都以 `hwnd` 为目标。不是时返回找到的
/// `(usage page, usage, 目标窗口)`（没有注册的项目标为 0），供日志说明是谁改走了。
pub(crate) fn registration_ok(hwnd: HWND) -> Result<(), Vec<(u16, u16, isize)>> {
    let mut buf = [RAWINPUTDEVICE::default(); 32];
    let mut n = buf.len() as u32;
    // SAFETY: 缓冲区与个数有效；cbSize 为单个结构大小。
    let got =
        unsafe { GetRegisteredRawInputDevices(Some(buf.as_mut_ptr()), &mut n, size_of::<RAWINPUTDEVICE>() as u32) };
    let regs: &[RAWINPUTDEVICE] = if got == u32::MAX {
        &[]
    } else {
        &buf[..(got as usize).min(buf.len())]
    };
    let wanted = devices(hwnd, RIDEV_INPUTSINK);
    let found: Vec<(u16, u16, isize)> = wanted
        .iter()
        .map(|w| {
            let t = regs
                .iter()
                .find(|r| r.usUsagePage == w.usUsagePage && r.usUsage == w.usUsage)
                .map_or(0, |r| r.hwndTarget.0 as isize);
            (w.usUsagePage, w.usUsage, t)
        })
        .collect();
    if found.iter().all(|&(_, _, t)| t == hwnd.0 as isize) {
        Ok(())
    } else {
        Err(found)
    }
}

pub(crate) fn unregister() {
    let devs = devices(HWND::default(), RIDEV_REMOVE);
    // SAFETY: 同上；RIDEV_REMOVE 要求 hwndTarget 为 NULL。
    unsafe {
        let _ = RegisterRawInputDevices(&devs, size_of::<RAWINPUTDEVICE>() as u32);
    }
}

#[derive(Debug, Default)]
pub(crate) struct RawTracker {
    /// 每个触摸板设备的 preparsed data（读取失败为 None）。
    preparsed: HashMap<isize, Option<Vec<u64>>>,
    buf: Vec<u64>,
    pub motion: Option<(f64, f64)>,
    /// 最近一次「有触点」的触摸板报告。
    pub last_touch: Option<Instant>,
    pub last_contacts: u32,
    /// 最近一次鼠标、键盘的 Raw Input（看门狗分别用）。
    pub last_mouse: Option<Instant>,
    pub last_kbd: Option<Instant>,
    /// A device report for which a legacy LL mouse callback is expected. Raw
    /// motion at a one-pixel clip and device-less SendInput reports are not proof
    /// that Windows removed the hook.
    pub last_mouse_hook_evidence: Option<Instant>,
    pub mouse_reports: u64,
    pub mouse_without_device: u64,
    pub keyboard_reports: u64,
    pub touch_reports: u64,
    pub mouse_kbd_reports: u64,
}

impl RawTracker {
    pub(crate) fn touch_active(&self, now: Instant) -> bool {
        self.last_touch
            .is_some_and(|t| now.saturating_duration_since(t) < TOUCH_WINDOW)
    }

    pub(crate) fn on_input(&mut self, lparam: LPARAM, now: Instant, clipped: bool, _remote: bool) {
        // SAFETY: reads the timestamp of the WM_INPUT currently dispatched.
        let message_time = unsafe { GetMessageTime() } as u32;
        super::wheel_origin::dispatched(lparam.0);
        let h = HRAWINPUT(lparam.0 as _);
        let header = size_of::<RAWINPUTHEADER>() as u32;
        let mut size = 0u32;
        // SAFETY: 第一次只取大小；第二次缓冲区（8 字节对齐）按大小分配。
        unsafe {
            if GetRawInputData(h, RID_INPUT, None, &mut size, header) != 0 || size == 0 {
                return;
            }
            let words = (size as usize).div_ceil(8);
            if self.buf.len() < words {
                self.buf.resize(words, 0);
            }
            let p = self.buf.as_mut_ptr().cast::<core::ffi::c_void>();
            if GetRawInputData(h, RID_INPUT, Some(p), &mut size, header) == u32::MAX {
                return;
            }
            let raw = &*(p as *const RAWINPUT);
            super::wheel_origin::processed(raw, message_time);
            if raw.header.dwType == RIM_TYPEMOUSE.0 {
                let mouse = raw.data.mouse;
                let has_device = !raw.header.hDevice.is_invalid();
                self.mouse_reports += 1;
                self.mouse_without_device += u64::from(!has_device);
                if mouse_hook_expected(
                    has_device,
                    mouse.lLastX != 0 || mouse.lLastY != 0,
                    mouse.Anonymous.Anonymous.usButtonFlags != 0,
                    clipped,
                ) {
                    self.last_mouse_hook_evidence = Some(now);
                }
                // Absolute tablet reports and synthetic input are not raw mouse counts.
                if raw_motion_capture_eligible(mouse.usFlags.0, has_device, mouse.ulExtraInformation) {
                    super::pointer_capture::record_motion(crate::pointer_capture::windows::nonzero_motion(
                        f64::from(mouse.lLastX),
                        f64::from(mouse.lLastY),
                    ));
                    self.motion = Some((f64::from(mouse.lLastX), f64::from(mouse.lLastY)));
                }
                self.last_mouse = Some(now);
                self.mouse_kbd_reports += 1;
                return;
            }
            if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
                self.keyboard_reports += 1;
                if !raw.header.hDevice.is_invalid() {
                    self.last_kbd = Some(now);
                }
                self.mouse_kbd_reports += 1;
                return;
            }
            if raw.header.dwType != RIM_TYPEHID.0 {
                return;
            }
            self.touch_reports += 1;
            let hid: &RAWHID = &raw.data.hid;
            let (each, count) = (hid.dwSizeHid as usize, hid.dwCount as usize);
            let data_off = std::ptr::addr_of!(hid.bRawData) as usize - raw as *const RAWINPUT as usize;
            if data_off + each * count > size as usize {
                return;
            }
            let reports = std::slice::from_raw_parts(std::ptr::addr_of!(hid.bRawData).cast::<u8>(), each * count);
            let device = raw.header.hDevice;
            let Some(pp) = self.preparsed_for(device) else {
                // 读不到 preparsed data：只要有报告就算有手指
                self.last_touch = Some(now);
                return;
            };
            let mut touched = None;
            for report in reports.chunks_exact(each.max(1)) {
                let mut contacts = 0u32;
                let st = HidP_GetUsageValue(
                    HidP_Input,
                    PAGE_DIGITIZER,
                    None,
                    USAGE_CONTACT_COUNT,
                    &mut contacts,
                    PHIDP_PREPARSED_DATA(pp.as_ptr() as isize),
                    report,
                );
                if st == HIDP_STATUS_SUCCESS && contacts > 0 {
                    touched = Some(contacts);
                }
            }
            if let Some(contacts) = touched {
                self.last_touch = Some(now);
                self.last_contacts = contacts;
            }
        }
    }

    fn preparsed_for(&mut self, device: HANDLE) -> Option<&Vec<u64>> {
        self.preparsed
            .entry(device.0 as isize)
            .or_insert_with(|| {
                let mut size = 0u32;
                // SAFETY: 第一次取大小，第二次按大小分配。
                unsafe {
                    GetRawInputDeviceInfoW(Some(device), RIDI_PREPARSEDDATA, None, &mut size);
                    if size == 0 {
                        return None;
                    }
                    let mut v = vec![0u64; (size as usize).div_ceil(8)];
                    let r = GetRawInputDeviceInfoW(
                        Some(device),
                        RIDI_PREPARSEDDATA,
                        Some(v.as_mut_ptr().cast()),
                        &mut size,
                    );
                    (r != u32::MAX).then_some(v)
                }
            })
            .as_ref()
    }
}

fn raw_motion_capture_eligible(flags: u16, has_device: bool, extra: u32) -> bool {
    flags & 1 == 0
        && has_device
        && extra as usize != super::inject::INJECT_TAG
        && !super::hook::is_marker(extra as usize)
}

fn mouse_hook_expected(has_device: bool, moved: bool, buttons_or_wheel: bool, clipped: bool) -> bool {
    has_device && (buttons_or_wheel || (moved && !clipped))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::{mouse_hook_expected, raw_motion_capture_eligible};

    #[test]
    fn own_raw_mouse_reports_never_enter_the_relative_capture_stream() {
        let own = super::super::inject::INJECT_TAG as u32;
        assert!(!raw_motion_capture_eligible(0, true, own));
        assert!(!raw_motion_capture_eligible(0, false, own));
        assert!(!raw_motion_capture_eligible(0, true, 0x0001_4e57));
        assert!(raw_motion_capture_eligible(0, true, 0));
        assert!(raw_motion_capture_eligible(0, true, 0x1234));
        assert!(!raw_motion_capture_eligible(1, true, 0x1234));
    }

    #[test]
    fn watchdog_requires_a_report_that_can_produce_a_mouse_hook_callback() {
        assert!(mouse_hook_expected(true, true, false, false));
        assert!(!mouse_hook_expected(true, true, false, true));
        // Confinement does not suppress button/wheel callbacks, so a genuinely
        // lost mouse hook can still be detected while playing a remote game.
        assert!(mouse_hook_expected(true, false, true, true));
        assert!(mouse_hook_expected(true, true, true, true));
        // Our own SendInput may bypass our hook; status-only packets do not
        // represent a mouse action at all. Neither authorizes reinstallation.
        assert!(!mouse_hook_expected(false, true, true, false));
        assert!(!mouse_hook_expected(true, false, false, false));
    }
}
