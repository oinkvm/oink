//! 注入：SendInput。所有注入的事件都带 [`INJECT_TAG`]（`dwExtraInfo`），钩子据此跳过自己注入的事件。
//!
//! - 移动：`MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`，坐标归一化到虚拟桌面的
//!   0..=65535（见 [`normalize`]）；绝对定位不经过「提高指针精确度」加速，按钮按住时产生拖动。
//! - 按钮：左 / 右 / 中 / X1（后退）/ X2（前进）。
//! - 滚动：`MOUSEEVENTF_WHEEL` / `MOUSEEVENTF_HWHEEL`。`Line` 是滚轮格数（`oink_foundation::input::ScrollUnit`），
//!   1.0 = WHEEL_DELTA（120）；`Pixel` 按 [`PIXELS_PER_LINE_DIP`] 先换成行，再按本机「每格行数」换成格；不足 1 个单位的小数部分累积到下次（高精度滚轮，`mouseData` 可小于 120）。
//!   符号：`dy > 0`（视图向上）→ 正的 WHEEL；`dx > 0`（视图向左）→ 负的 HWHEEL。注入的滚轮不经过鼠标驱动，
//!   不受 `FlipFlopWheel` 影响，也不受触摸板方向设置影响。
//! - 按键：扫描码注入（`KEYEVENTF_SCANCODE`，E0 扩展键加 `KEYEVENTF_EXTENDEDKEY`）；Pause 与媒体键用虚拟键。

use oink_foundation::input::{KeyCode, MouseButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    VIRTUAL_KEY,
};

use super::keymap;

/// Oink-originated input marker ("OINK"), shared with the service and volume exit.
pub(crate) const INJECT_TAG: usize = 0x4F49_4E4B;
/// 一格滚轮。
pub(crate) const WHEEL_DELTA: f64 = 120.0;
/// `Pixel` 单位滚动的换算：一行 = 100/3 个 DIP（Chromium 在 Windows 上的滚轮行高：一格 3 行 = 100 DIP）。
/// 物理像素先按目标屏 DPI 换算成 DIP（96 DPI = 1:1）。
pub(crate) const PIXELS_PER_LINE_DIP: f64 = 100.0 / 3.0;

/// 把像素坐标归一化到 0..=65535（虚拟桌面原点 `origin`、宽度 `extent`）。
///
/// 系统按 `pixel = origin + floor(n * extent / 65536)` 反算，这里取满足条件的最小 `n`（向上取整），
/// 在 Windows 上逐像素核对过。
pub(crate) fn normalize(pixel: i32, origin: i32, extent: i32) -> i32 {
    if extent <= 0 {
        return 0;
    }
    let rel = i64::from(pixel - origin).clamp(0, i64::from(extent) - 1);
    let n = (rel * 65536 + i64::from(extent) - 1) / i64::from(extent);
    n.clamp(0, 65535) as i32
}

pub(crate) fn drag_press_at(at: oink_foundation::Point, origin: (i32, i32), extent: (i32, i32)) -> INPUT {
    // Match MoveTo's containing pixel. Rounding a half-open edge position such
    // as y=-0.000001 to zero presses on the adjacent display instead of the
    // source window; positive right/bottom edges have the same failure.
    drag_press(
        normalize(at.x.floor() as i32, origin.0, extent.0),
        normalize(at.y.floor() as i32, origin.1, extent.1),
    )
}

/// 滚动量 → WHEEL 单位（小数）。
/// 滚轮格数 → WHEEL 单位（`ScrollUnit::Line` 注入）。
pub(crate) fn notch_to_wheel(notches: f64) -> f64 {
    notches * WHEEL_DELTA
}

pub(crate) fn line_to_wheel(lines: f64, local_lines_per_notch: f64) -> f64 {
    if local_lines_per_notch <= 0.0 {
        return 0.0; // 用户关闭了滚轮滚动
    }
    lines / local_lines_per_notch * WHEEL_DELTA
}

pub(crate) fn pixel_to_wheel(px: f64, dpi: u32, local_lines_per_notch: f64) -> f64 {
    let dip = px * 96.0 / f64::from(dpi.max(1));
    line_to_wheel(dip / PIXELS_PER_LINE_DIP, local_lines_per_notch)
}

/// 累积小数部分，返回本次应发出的整数单位。
pub(crate) fn take_whole(acc: &mut f64, add: f64) -> i32 {
    // 容差：0.3 / 3 * 120 = 11.999999999999998 这类浮点误差不能让一个单位永远留在余数里
    const EPS: f64 = 1e-6;
    *acc += add;
    let whole = if *acc >= 0.0 {
        (*acc + EPS).floor()
    } else {
        (*acc - EPS).ceil()
    };
    *acc -= whole;
    whole as i32
}

pub(crate) fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

pub(crate) fn move_abs(nx: i32, ny: i32) -> INPUT {
    mouse(
        nx,
        ny,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )
}

/// One positioned Down. A separate move then button permits physical input to
/// intervene; SendInput inserts this INPUT as one event. The protected helper
/// carries the same combined event via DragPress, rather than splitting it.
pub(crate) fn drag_press(nx: i32, ny: i32) -> INPUT {
    mouse(
        nx,
        ny,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_LEFTDOWN,
    )
}

pub(crate) fn wheel(units: i32, horizontal: bool) -> INPUT {
    mouse(
        0,
        0,
        units,
        if horizontal {
            MOUSEEVENTF_HWHEEL
        } else {
            MOUSEEVENTF_WHEEL
        },
    )
}

pub(crate) fn button(b: MouseButton, pressed: bool) -> Option<INPUT> {
    const XBUTTON1: i32 = 1;
    const XBUTTON2: i32 = 2;
    let (flags, data) = match (b, pressed) {
        (MouseButton::Left | MouseButton::Other(0), true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left | MouseButton::Other(0), false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right | MouseButton::Other(1), true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right | MouseButton::Other(1), false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle | MouseButton::Other(2), true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle | MouseButton::Other(2), false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::Back | MouseButton::Other(3), true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (MouseButton::Back | MouseButton::Other(3), false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (MouseButton::Forward | MouseButton::Other(4), true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (MouseButton::Forward | MouseButton::Other(4), false) => (MOUSEEVENTF_XUP, XBUTTON2),
        (MouseButton::Other(_), _) => return None,
    };
    Some(mouse(0, 0, data, flags))
}

/// 菜单遮罩键（`KeyCode::MENU_MASK`）注入的虚拟键：0xE8 未分配，不产生字符、没有系统含义，
/// 但能让系统知道 Alt / Win 按着期间按过别的键（不再激活菜单栏、打开开始菜单）。
const VK_MENU_MASK: u16 = 0xE8;

pub(crate) fn key(code: KeyCode, pressed: bool) -> Option<INPUT> {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    if code == KeyCode::MENU_MASK {
        return Some(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(VK_MENU_MASK),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECT_TAG,
                },
            },
        });
    }
    let e = keymap::lookup_hid(code)?;
    let extended = e.scancode >> 8 == 0xE0;
    let (vk, scan) = if e.vk != 0 {
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        (VIRTUAL_KEY(e.vk), e.scancode & 0xFF)
    } else {
        flags |= KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        (VIRTUAL_KEY(0), e.scancode & 0xFF)
    };
    Some(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    })
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    /// 系统的反算公式。
    fn denormalize(n: i32, origin: i32, extent: i32) -> i32 {
        origin + ((i64::from(n) * i64::from(extent)) >> 16) as i32
    }

    #[test]
    fn normalize_hits_every_pixel() {
        for (origin, extent) in [
            (0, 3840),
            (-1920, 5760),
            (-2560, 2560 + 3840),
            (0, 1),
            (0, 1366),
            (-3000, 65535),
        ] {
            for p in (origin..origin + extent).step_by(1.max(extent as usize / 5000)) {
                let n = normalize(p, origin, extent);
                assert!((0..=65535).contains(&n));
                assert_eq!(
                    denormalize(n, origin, extent),
                    p,
                    "origin {origin} extent {extent} pixel {p}"
                );
            }
            // 边界外的点被夹到边上
            assert_eq!(
                denormalize(normalize(origin - 10, origin, extent), origin, extent),
                origin
            );
            assert_eq!(
                denormalize(normalize(origin + extent + 10, origin, extent), origin, extent),
                origin + extent - 1
            );
        }
    }

    #[test]
    fn wheel_conversion() {
        assert_eq!(notch_to_wheel(1.0), 120.0);
        assert_eq!(notch_to_wheel(-0.25), -30.0);
        assert_eq!(line_to_wheel(3.0, 3.0), 120.0);
        assert_eq!(line_to_wheel(1.0, 3.0), 40.0);
        assert_eq!(line_to_wheel(3.0, 6.0), 60.0);
        assert_eq!(line_to_wheel(3.0, 0.0), 0.0);
        // 100 DIP = 一格（3 行/格）
        assert!((pixel_to_wheel(100.0, 96, 3.0) - 120.0).abs() < 1e-9);
        // 150% 缩放：150 物理像素 = 100 DIP
        assert!((pixel_to_wheel(150.0, 144, 3.0) - 120.0).abs() < 1e-9);
    }

    #[test]
    fn accumulates_fractions() {
        let mut acc = 0.0;
        let sent: i32 = (0..10).map(|_| take_whole(&mut acc, 12.5)).sum();
        assert_eq!(sent, 125);
        assert!(acc.abs() < 1e-9);
        // 0.1 格 × 10 = 一格（浮点误差不丢单位）
        let mut acc = 0.0;
        let sent: i32 = (0..10).map(|_| take_whole(&mut acc, line_to_wheel(0.3, 3.0))).sum();
        assert_eq!(sent, 120);
        let mut acc = 0.0;
        assert_eq!(take_whole(&mut acc, -0.6), 0);
        assert_eq!(take_whole(&mut acc, -0.6), -1);
    }

    #[test]
    fn buttons() {
        let data = |i: INPUT| {
            // SAFETY: 读取刚构造的联合体的 mi 成员。
            unsafe {
                (
                    i.Anonymous.mi.dwFlags,
                    i.Anonymous.mi.mouseData,
                    i.Anonymous.mi.dwExtraInfo,
                )
            }
        };
        assert_eq!(
            data(button(MouseButton::Back, true).unwrap()),
            (MOUSEEVENTF_XDOWN, 1, INJECT_TAG)
        );
        assert_eq!(
            data(button(MouseButton::Forward, false).unwrap()),
            (MOUSEEVENTF_XUP, 2, INJECT_TAG)
        );
        assert_eq!(
            data(button(MouseButton::Other(0), true).unwrap()).0,
            MOUSEEVENTF_LEFTDOWN
        );
        assert!(button(MouseButton::Other(7), true).is_none());
    }

    #[test]
    fn source_press_is_one_positioned_down() {
        let input = drag_press(123, 456);
        // SAFETY: drag_press constructs INPUT_MOUSE and initializes the mi member.
        let mouse = unsafe { input.Anonymous.mi };
        assert_eq!((mouse.dx, mouse.dy), (123, 456));
        assert_eq!(
            mouse.dwFlags,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_LEFTDOWN
        );
        assert_eq!(mouse.dwExtraInfo, INJECT_TAG);
    }

    #[test]
    fn source_press_keeps_fractional_edges_on_the_containing_display() {
        use oink_foundation::{Point, Rect};
        let screens = [
            Rect::new(0., 0., 1920., 1080.),
            Rect::new(1560., -1080., 1920., 1080.),
            Rect::new(-1920., 0., 1920., 1080.),
        ];
        let (origin, extent) = ((-1920, -1080), (5400, 2160));
        for (at, expected) in [
            (Point::new(2500.25, -0.000001), (2500, -1)),
            (Point::new(-0.000001, 500.25), (-1, 500)),
            (Point::new(1919.999999, 500.25), (1919, 500)),
            (Point::new(1000.25, 1079.999999), (1000, 1079)),
            (Point::new(3479.999999, -500.25), (3479, -501)),
        ] {
            let input = drag_press_at(at, origin, extent);
            // SAFETY: drag_press_at constructs INPUT_MOUSE without submitting it.
            let mouse = unsafe { input.Anonymous.mi };
            let pixel = (
                denormalize(mouse.dx, origin.0, extent.0),
                denormalize(mouse.dy, origin.1, extent.1),
            );
            assert_eq!(pixel, expected, "{at:?}");
            let display = screens.iter().position(|b| b.contains(at));
            assert_eq!(
                screens
                    .iter()
                    .position(|b| b.contains(Point::new(pixel.0.into(), pixel.1.into()))),
                display,
                "a positioned DOWN must stay on the display selected by MoveTo"
            );
        }
    }

    #[test]
    fn keys() {
        let ki = |i: INPUT| {
            // SAFETY: 读取刚构造的联合体的 ki 成员。
            unsafe { i.Anonymous.ki }
        };
        let a = ki(key(KeyCode::keyboard(0x04), true).unwrap());
        assert_eq!((a.wScan, a.dwFlags), (0x1E, KEYEVENTF_SCANCODE));
        let right = ki(key(KeyCode::keyboard(0x4F), false).unwrap());
        assert_eq!(right.wScan, 0x4D);
        assert_eq!(
            right.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
        let pause = ki(key(KeyCode::keyboard(0x48), true).unwrap());
        assert_eq!((pause.wVk, pause.dwFlags), (VIRTUAL_KEY(0x13), KEYBD_EVENT_FLAGS(0)));
        let mute = ki(key(KeyCode::new(KeyCode::PAGE_CONSUMER, 0xE2), true).unwrap());
        assert_eq!((mute.wVk, mute.dwFlags), (VIRTUAL_KEY(0xAD), KEYEVENTF_EXTENDEDKEY));
        // 菜单遮罩键：未分配的虚拟键 0xE8，不带扫描码。
        let mask = ki(key(KeyCode::MENU_MASK, true).unwrap());
        assert_eq!(
            (mask.wVk, mask.wScan, mask.dwFlags),
            (VIRTUAL_KEY(0xE8), 0, KEYBD_EVENT_FLAGS(0))
        );
        let mask_up = ki(key(KeyCode::MENU_MASK, false).unwrap());
        assert_eq!(mask_up.dwFlags, KEYEVENTF_KEYUP);
        for pressed in [true, false] {
            let caps = ki(key(KeyCode::CAPS_LOCK, pressed).unwrap());
            assert_eq!(caps.wScan, 0x3A);
            assert_eq!(caps.wVk.0, 0);
            assert_eq!(caps.dwFlags.contains(KEYEVENTF_KEYUP), !pressed);
            assert!(caps.dwFlags.contains(KEYEVENTF_SCANCODE));
            assert_eq!(caps.dwExtraInfo, INJECT_TAG);
        }
        assert!(key(KeyCode::keyboard(0x01), true).is_none());
    }
}
