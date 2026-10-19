//! 路由的配置：本机身份（启动时确定）与用户设置（保存在设置文件里，界面可改）。

use oink_foundation::DeviceId;
use oink_foundation::input::ScrollUnit;
use serde::{Deserialize, Serialize};

/// 设备的操作系统（用于输入转换默认值、滚动注入单位等）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Os {
    MacOS,
    Windows,
    Other,
}

impl Os {
    /// 本机的系统。
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Os::MacOS
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Other
        }
    }

    /// `DeviceFacts::os` 里的写法。
    pub fn as_str(self) -> &'static str {
        match self {
            Os::MacOS => "macos",
            Os::Windows => "windows",
            Os::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "macos" => Os::MacOS,
            "windows" => Os::Windows,
            _ => Os::Other,
        }
    }
}

/// 焦点移交时有鼠标按钮按着（例如正在拖动）怎么办。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ButtonHandoff {
    /// 不移交：通往其他设备的边在按钮抬起前表现为墙。按钮的抬起一定回到按下它的那台设备。
    Block,
    /// 照常移交（按钮仍按「按下时的目标即抬起时的目标」处理，不会卡住；
    /// 但跨设备拖放要等剪贴板模块支持，目前拖到另一台设备上不会带着内容）。
    Allow,
}

/// 心跳超时的下限（毫秒）。组网换链路时最长有 1 s 的静默宽限期（`oink_network` 的
/// `Timings::link_grace`），超时必须比它长，否则换一条链路路由就判对端超时、收回焦点。
/// 设置里更小的值（早期版本把 1000 写进了 `settings.json`）按这个值处理。
pub const MIN_HEARTBEAT_TIMEOUT_MS: u32 = 1500;

/// 紧急暂停的默认长按时长（毫秒）。
pub const DEFAULT_EMERGENCY_HOLD_MS: u32 = 5000;

/// 用户可改的路由设置（设置文件 `settings.json` 的 `routing` 部分）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingSettings {
    pub mouse_mode: oink_foundation::input::InputMode,
    pub keyboard_mode: oink_foundation::input::InputMode,
    pub heartbeat_interval_ms: u32,
    pub heartbeat_timeout_ms: u32,
    pub button_handoff: ButtonHandoff,
    /// 一行的物理高度（plane），用于行滚动与像素滚动互换（「行 × 源机典型行高」）。
    pub line_height_ratio: f64,
    /// 本机作为目标时注入滚动的单位；`None` 按系统：macOS 像素、Windows 行（滚轮格）。
    pub scroll_output: Option<ScrollUnit>,
    /// 本机作为源时换算位移用的参考屏（`DisplayInfo::key`）；`None` = 指针离开本机时所在的屏幕。
    pub fixed_reference_display: Option<String>,
    /// 物理尺寸未知的屏幕按这个 reference_per_native_unit 换算。
    pub fallback_reference_per_native_unit: f64,
    /// 已不是焦点机的设备最多再转发几次（防止环路）。
    pub max_forward_hops: u8,
    /// 本机作为目标时注入远端滚动的倍率（界面「滚动速度」，默认 1）。
    pub remote_scroll_speed: f64,
    /// 紧急暂停：在本机键盘上按住 Esc 多久（毫秒）暂停整个设备组的
    /// 共享；0 表示关闭长按触发。
    pub emergency_hold_ms: u32,
    /// 紧急暂停的备选组合键 Ctrl + Alt（Option）+ Shift + Esc。
    pub emergency_combo: bool,
    /// 触摸板手势转发给正在操作的那台（界面「触摸板手势」开关，默认开）。
    /// 关闭时焦点在其他设备上的手势吞掉、不转发（本机也不响应）；焦点在本机时照常。不影响媒体键。
    pub forward_gestures: bool,
    /// Runtime projection of the receiving owner's AcceptContacts preference.
    #[serde(skip)]
    pub accept_contacts: bool,
    /// Runtime projection of this device owner's persistent game preference.
    #[serde(skip)]
    pub game_mode_preference: crate::GameMode,
    /// Return remotely controlled focus to its pointer source when all local displays sleep.
    pub return_pointer_on_display_sleep: bool,
    /// Respond to group user activity by waking displays.
    pub wake_displays_on_input: bool,
    /// Enable drag handoffs between devices; enabled by default.
    pub dnd: bool,
    /// 拖放推边距离占离开屏幕对角线的比例（本机点 / 像素）。
    pub dnd_push_fraction: f64,
    /// Length on each adjacent edge reserved for an active corner, in physical screen diagonals.
    pub edge_resistance_corner_fraction: f64,
    /// Platform damping profile and optional manual edge segments.
    pub damping: crate::damping::DampingConfig,
    /// Show a cursor trail after a quick circle gesture; enabled by default.
    /// Runtime projection of the local owner preference.
    #[serde(skip)]
    pub cursor_trail: bool,
    #[serde(skip)]
    pub cursor_trail_duration_ms: u32,
    #[serde(skip)]
    pub event_frequency: oink_display::layout::EventFrequency,
}

impl Default for RoutingSettings {
    fn default() -> Self {
        Self {
            mouse_mode: Default::default(),
            keyboard_mode: Default::default(),
            heartbeat_interval_ms: 250,
            heartbeat_timeout_ms: MIN_HEARTBEAT_TIMEOUT_MS,
            button_handoff: ButtonHandoff::Block,
            line_height_ratio: 8.0 / 686.0,
            scroll_output: None,
            fixed_reference_display: None,
            fallback_reference_per_native_unit: 0.25 / 686.0,
            max_forward_hops: 2,
            remote_scroll_speed: 1.0,
            emergency_hold_ms: DEFAULT_EMERGENCY_HOLD_MS,
            emergency_combo: true,
            forward_gestures: true,
            accept_contacts: true,
            game_mode_preference: crate::GameMode::Auto,
            return_pointer_on_display_sleep: true,
            wake_displays_on_input: true,
            dnd: true,
            dnd_push_fraction: 0.024,
            edge_resistance_corner_fraction: 0.005,
            damping: Default::default(),
            cursor_trail: true,
            cursor_trail_duration_ms: oink_display::layout::DEFAULT_CURSOR_TRAIL_DURATION_MS,
            event_frequency: oink_display::layout::EventFrequency::default(),
        }
    }
}

/// 路由的完整配置。
#[derive(Clone, Debug, PartialEq)]
pub struct RouterConfig {
    pub device: DeviceId,
    pub boot: oink_foundation::session::Boot,
    pub device_name: String,
    pub os: Os,
    pub app_version: String,
    pub settings: RoutingSettings,
    /// 本机的截获是否上报系统的按键自动重复（已按下的键再次「按下」）。
    ///
    /// - `true`（默认；macOS、Windows backend 都上报）：已按下的键再次按下视为系统的自动重复：发往本机的
    ///   放行，发往远端的转发为 `KeyRepeat`（重复只在源机产生）。截获漏掉的抬起由 backend
    ///   补报（macOS：非自动重复的按下遇到仍按着的键时先补报抬起，按着键期间按物理状态核对）。
    /// - `false`（不上报自动重复的截获，例如测试用的假输入）：已按下的键再次按下说明**上一次的抬起丢了**
    ///   (for example, an event-tap timeout). Release the recorded target first,
    ///   再当作新的按下处理。
    pub capture_reports_key_repeat: bool,
}

impl RouterConfig {
    pub fn new(boot: oink_foundation::session::Boot, device_name: impl Into<String>, os: Os) -> Self {
        Self {
            device: boot.device_id,
            boot,
            device_name: device_name.into(),
            os,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            settings: RoutingSettings::default(),
            capture_reports_key_repeat: true,
        }
    }
}

impl RoutingSettings {
    pub(crate) fn edge_damping_profile(&self) -> crate::damping::DampingProfile {
        self.damping.platform_profile
    }
}
