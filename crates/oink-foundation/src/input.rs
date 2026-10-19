//! 输入事件模型：平台实现截获后翻译成这里的类型，经路由与网络传给目标电脑，再由目标的平台实现注入。
//!
//! Shared events and key codes preserve the source device's input semantics.

use crate::geom::Point;
use serde::{Deserialize, Serialize};

/// Policy of this device's physical input sources, independent of group focus.
/// Keyboard disable is a separate runtime guard, never an InputMode value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    Shared,
    Local,
}

/// Source monotonic capture clock, in wrapping microseconds. This is interval
/// metadata, never an absolute remote deadline or an injection timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureTime(pub u32);
impl CaptureTime {
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros as u32)
    }
    /// Only intervals shorter than half the wrap period are unambiguous. Consumers
    /// must also reset after a long local idle or source/connection change.
    pub const fn elapsed_since(self, previous: Self) -> Option<u32> {
        let delta = self.0.wrapping_sub(previous.0);
        if delta < (1 << 31) { Some(delta) } else { None }
    }
}

/// 键码：HID Usage（`usage_page << 16 | usage_id`），与平台无关。键盘按键在 Keyboard/Keypad 页（0x07），
/// 媒体键在 Consumer 页（0x0C）。平台实现负责本机扫描码/虚拟键码与它的互转。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct KeyCode(pub u32);

impl KeyCode {
    pub const PAGE_KEYBOARD: u16 = 0x07;
    pub const PAGE_CONSUMER: u16 = 0x0C;

    pub const fn new(page: u16, usage: u16) -> Self {
        Self(((page as u32) << 16) | usage as u32)
    }

    pub const fn keyboard(usage: u16) -> Self {
        Self::new(Self::PAGE_KEYBOARD, usage)
    }

    pub const fn page(self) -> u16 {
        (self.0 >> 16) as u16
    }

    pub const fn usage(self) -> u16 {
        (self.0 & 0xffff) as u16
    }

    /// Volume controls supported on both HID usage pages.
    pub const fn is_volume(self) -> bool {
        (self.page() == Self::PAGE_CONSUMER && matches!(self.usage(), 0xE2 | 0xE9 | 0xEA))
            || (self.page() == Self::PAGE_KEYBOARD && matches!(self.usage(), 0x7F..=0x81))
    }

    pub const fn is_modifier(self) -> bool {
        self.page() == Self::PAGE_KEYBOARD && self.usage() >= 0xE0 && self.usage() <= 0xE7
    }

    pub const LEFT_CTRL: Self = Self::keyboard(0xE0);
    pub const LEFT_SHIFT: Self = Self::keyboard(0xE1);
    /// Windows 上的 Alt，Mac 上的 Option。
    pub const LEFT_ALT: Self = Self::keyboard(0xE2);
    /// Windows 键 / Mac Command 键。
    pub const LEFT_GUI: Self = Self::keyboard(0xE3);
    pub const RIGHT_CTRL: Self = Self::keyboard(0xE4);
    pub const RIGHT_SHIFT: Self = Self::keyboard(0xE5);
    pub const RIGHT_ALT: Self = Self::keyboard(0xE6);
    pub const RIGHT_GUI: Self = Self::keyboard(0xE7);
    pub const CAPS_LOCK: Self = Self::keyboard(0x39);
    pub const ESCAPE: Self = Self::keyboard(0x29);

    /// Oink 私有的「菜单遮罩」键（厂商自定义页，不是真实按键）。Windows 上单独按下又松开 Alt 会激活应用的
    /// 菜单栏、单独按下又松开 Win 键会打开开始菜单；路由吞掉了 Alt / Win 组合里的普通键（快捷键），或让按着的
    /// Alt / Win 跟随焦点到另一台设备时，在它们松开之前给同一个目标补一次这个键的按下与抬起。Windows 目标注入
    /// 未分配的虚拟键 0xE8（与 AutoHotkey 的 menu mask 相同，不产生字符、没有系统含义），其他系统忽略。
    pub const MENU_MASK: Self = Self::new(0xFFAC, 0x0001);
}

/// 事件来自哪一类设备。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceKind {
    Mouse,
    Touchpad,
    Keyboard,
    /// 无法判断（例如被其他软件合成的事件）。
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    /// 侧键「后退」（按钮 4）。
    Back,
    /// 侧键「前进」（按钮 5）。
    Forward,
    /// 其他按钮，数值为从 0 开始的按钮编号。
    Other(u8),
}

/// 滚动量的单位。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollUnit {
    /// 源机本地坐标单位（mac point / Win px），已含源系统的滚动加速；触摸板和 mac 鼠标通常是这种。
    Pixel,
    /// 滚轮格数（notch）。Windows：`mouseData / WHEEL_DELTA`（高精度滚轮为小数），注入时 1.0 = `WHEEL_DELTA`；
    /// macOS：按行计（macOS 没有「每格行数」设置，一格即一行）。
    /// 与「行」的换算（× `FeelProfile::wheel_lines_per_notch`）由输入路由负责，平台实现不做。
    Line,
}

/// 滚动阶段（主要来自 macOS 触摸板；没有阶段信息的滚轮为 `None`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollPhase {
    None,
    Began,
    Changed,
    Ended,
    Cancelled,
    MomentumBegan,
    Momentum,
    MomentumEnded,
}

/// 一个输入事件（与平台无关）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Local raw gesture observation, never paced as pointer motion.
    TouchpadSample(crate::contacts::Sample),
    /// 指针相对位移。**已经过源系统的速度与加速处理**，单位为源机本地坐标单位。
    PointerDelta {
        dx: f64,
        dy: f64,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    /// 滚动。符号约定（与各系统的「自然滚动」设置无关）：
    /// **`dy > 0` 表示视图向文档顶部移动**（传统滚轮向上滚）；**`dx > 0` 表示视图向文档左侧移动**。
    /// 源端已按源设备的方向偏好换算；目标端注入时要保证产生同样的视图移动方向。
    Scroll {
        dx: f64,
        dy: f64,
        unit: ScrollUnit,
        phase: ScrollPhase,
    },
    Key {
        code: KeyCode,
        pressed: bool,
    },
    /// 触摸板手势（macOS 源机；追加的变体）。见 [`Gesture`]。
    Gesture(Gesture),
    /// Unaccelerated device counts, reported alongside PointerDelta. Routing uses only
    /// this stream in game mode; ordinary routing ignores it.
    RawPointerDelta {
        dx: f64,
        dy: f64,
    },
}

/// 手势阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GesturePhase {
    Began,
    Changed,
    Ended,
    Cancelled,
}

impl GesturePhase {
    /// 手势到此结束（结束或取消）。
    pub const fn is_end(self) -> bool {
        matches!(self, Self::Ended | Self::Cancelled)
    }
}

/// Dock 轻扫（系统级的三 / 四指手势，由 macOS 的 Dock 处理）的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwipeAxis {
    /// 左右轻扫：切换 Space / 全屏应用。
    Horizontal,
    /// 上下轻扫：调度中心（向上）/ App Exposé（向下）。
    Vertical,
    /// 捏合 / 张开：启动台（macOS 26 为 Apps）/ 显示桌面。
    Pinch,
}

/// 触摸板手势。macOS 源机截获系统已识别好的手势，目标是 Mac 时原样重放
/// （跟手的交互式进度），目标是 Windows 时映射为快捷键（任务视图、切换桌面、显示桌面、Ctrl+滚轮）。
/// 两指滚动与两指轻扫返回不在这里，走带阶段的 [`InputEvent::Scroll`]。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Gesture {
    /// Dock 轻扫。`progress`：这次手势从开始到现在的累计进度（1.0 约为
    /// 完成一次切换；Pinch 已归一化为正 = 张开、负 = 捏合）；`velocity`：结束时的速度（同一单位 / 秒，
    /// 只在 `Ended` 时有意义，Dock 据此决定是否完成）。
    DockSwipe {
        axis: SwipeAxis,
        phase: GesturePhase,
        progress: f64,
        velocity: f64,
    },
    /// 捏合缩放：这一步的放大量（`NSEvent.magnification`，0.1 = 放大 10%，负值缩小）。
    Magnify { phase: GesturePhase, amount: f64 },
    /// 旋转：这一步的角度（度，逆时针为正，同 `NSEvent.rotation`）。
    Rotate { phase: GesturePhase, degrees: f64 },
    /// 智能缩放（两指轻点两下）。
    SmartMagnify,
}

impl Gesture {
    /// 手势阶段；智能缩放是一次性的，为 `None`。
    pub const fn phase(&self) -> Option<GesturePhase> {
        match self {
            Self::DockSwipe { phase, .. } | Self::Magnify { phase, .. } | Self::Rotate { phase, .. } => Some(*phase),
            Self::SmartMagnify => None,
        }
    }

    /// 开始一次新手势（开始阶段，或一次性的手势）。
    pub const fn starts(&self) -> bool {
        matches!(self.phase(), None | Some(GesturePhase::Began))
    }

    /// 这次手势到此结束（结束、取消，或一次性的手势）。
    pub const fn ends(&self) -> bool {
        match self.phase() {
            None => true,
            Some(p) => p.is_end(),
        }
    }
}

/// 截获到的本机事件。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapturedEvent {
    /// Native physical input; false for synthetic/recovery events.
    #[serde(default)]
    pub physical: bool,
    pub event: InputEvent,
    pub source: SourceKind,
    /// 本机单调时钟，微秒。
    pub time_us: u64,
    /// 事件发生时的光标位置（本机全局坐标），**已应用本次移动之后**的位置（即系统将把光标放到的位置）。
    /// 仅指针类事件有值；`Remote` 模式下为 `None`（光标停在停靠点，不是用户指针的位置）。
    pub cursor: Option<Point>,
}

/// 截获回调的返回值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// 交还系统，本机照常处理。
    Pass,
    /// 吞掉，本机不再处理。
    Swallow,
}

/// A synchronous local rewrite decided after physical shortcuts and routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalRewrite {
    Key { code: KeyCode, pressed: bool },
    ReverseWheel,
}

/// 注入到本机的事件。普通指针用绝对位置避免二次加速；游戏模式使用 MoveRelative。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InjectEvent {
    /// Gesture-only physical contact changes, with a separate native device.
    TouchpadContacts(crate::contacts::Frame),
    /// 把光标移到本机全局坐标 `pos`；有按钮按下时平台实现应产生拖动事件。
    MoveTo {
        pos: Point,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    Scroll {
        dx: f64,
        dy: f64,
        unit: ScrollUnit,
        phase: ScrollPhase,
    },
    Key {
        code: KeyCode,
        pressed: bool,
    },
    /// 同 [`InjectEvent::MoveTo`]，另外给出这一步**被墙裁剪之前**的位移 `(dx, dy)`（本机坐标单位）。
    ///
    /// 指针被屏幕外缘挡住时位置不变、位移仍朝外。macOS 靠移动事件的位移（`kCGMouseEventDeltaX/Y`）
    /// 判断「推边」：自动隐藏的 Dock、全屏时的菜单栏都要看到朝外的位移才会出现，只看位置的差为 0 就不会
    /// 出现。只关心位置的平台实现（Windows）按 `MoveTo` 处理。
    MoveToWithDelta {
        pos: Point,
        dx: f64,
        dy: f64,
    },
    /// 触摸板手势（追加的变体）：macOS 重放，Windows 映射为快捷键，做不了的丢弃。
    Gesture(Gesture),
    /// Relative device counts; no layout or screen scaling.
    MoveRelative {
        dx: f64,
        dy: f64,
    },
    /// Directed volume event; modifier bits correspond to keyboard usages E0–E7.
    Volume {
        code: KeyCode,
        pressed: bool,
        repeat: bool,
        modifiers: u8,
    },
    /// Local Windows touchpad delivery in reference motion.
    /// `pixels_per_reference` describes the destination, and `wheel` supplies line delivery
    /// for missing APIs, secure desktops and injection failures. Never sent on the wire.
    TouchpadScroll {
        dx: f64,
        dy: f64,
        pixels_per_reference: f64,
        wheel: ScrollInjection,
    },
}

/// Existing scroll delivery, retained alongside precision touchpad metadata.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollInjection {
    pub dx: f64,
    pub dy: f64,
    pub unit: ScrollUnit,
    pub phase: ScrollPhase,
}

impl InjectEvent {
    /// 移动类事件（`MoveTo` / `MoveToWithDelta`）的目标位置。
    pub fn move_target(&self) -> Option<Point> {
        match self {
            Self::MoveTo { pos } | Self::MoveToWithDelta { pos, .. } => Some(*pos),
            _ => None,
        }
    }
}

/// 源设备的手感档案。由源机的平台实现读取，设置变化时重新推送给其他电脑。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeelProfile {
    /// 按键重复：首次重复前的延迟与之后的间隔（毫秒）。
    pub key_repeat_delay_ms: u32,
    pub key_repeat_interval_ms: u32,
    /// Windows 滚轮每格滚动的行数（`SPI_GETWHEELSCROLLLINES`）；macOS 为 `None`。
    pub wheel_lines_per_notch: Option<f64>,
    /// 源机对鼠标 / 触摸板是否启用「自然滚动」（仅供显示与诊断；方向已在事件里换算）。
    pub natural_scroll_mouse: bool,
    pub natural_scroll_touchpad: bool,
}

impl Default for FeelProfile {
    fn default() -> Self {
        Self {
            key_repeat_delay_ms: 500,
            key_repeat_interval_ms: 33,
            wheel_lines_per_notch: None,
            natural_scroll_mouse: false,
            natural_scroll_touchpad: false,
        }
    }
}

/// 本机光标的截获模式，与组焦点、键盘路由独立。mouse-local 始终使用 Local。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum CaptureMode {
    /// 焦点在本机：事件照常交给系统，截获回调只做观察（以及在屏幕边缘判定是否移交焦点）。
    Local,
    /// 焦点在其他电脑：光标隐藏并停在 `park`（本机全局坐标；macOS 实现不移动光标，见
    /// `oink_input::warp_delta`），指针位移以 `PointerDelta` 上报，
    /// 平台实现保证本机光标不动、不触发本机的悬停/边缘行为。macOS 立即隐藏、解耦并挪出感应区。
    Remote { park: Point },
}

/// 系统权限状态（界面「引导与权限」使用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionState {
    Granted,
    /// The platform positively reports a refusal, rather than just a failed preflight.
    Denied,
    /// 还没有询问过用户。
    NotDetermined,
    /// 本系统不需要这项权限。
    NotApplicable,
    /// Not authorized; the platform cannot establish whether the user was asked.
    NotGranted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputPermissions {
    /// macOS「辅助功能」：注入事件、截获并屏蔽事件需要。
    pub accessibility: PermissionState,
    /// macOS「输入监控」：截获键盘需要。
    pub input_monitoring: PermissionState,
    /// macOS「安全输入」当前是否被其他程序开启（开启时截获不到键盘）。
    pub secure_input_active: bool,
}

/// Platform-reported local screen edges reserved for system interactions.
/// Not sent over the wire; an empty list means immediate handoff everywhere.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EdgeResistanceRegion {
    pub display: String,
    pub edges: Vec<ScreenEdge>,
    pub corners: Vec<ScreenCorner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreenEdge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentAudioNotifications {
    /// Ages at the source key press, only within the preceding 5000ms.
    pub default_endpoint_age_ms: Option<u32>,
    pub arrival_removal_age_ms: Option<u32>,
}
