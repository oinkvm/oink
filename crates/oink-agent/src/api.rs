//! Typed local queries, commands, and events shared by the service, UI, and CLI.
//! [`AgentApi`] is implemented by the local Node and by the IPC client.
//! [`Request`] uses `{method, params}` and [`ApiEvent`] uses `{type, data}`.
//! Device-card fields use camelCase; other payloads follow their serde declarations.
//! Device and group identifiers are canonical strings. Owner revisions use decimal
//! strings to preserve their full range in JavaScript. Layout coordinates use
//! normalized reference units, while timestamps use Unix milliseconds.
//! Device-card TypeScript contracts live beside their Rust types in `device_cards`.

mod notifications;
pub mod pairing;
mod preference_patch;
mod preferences;
pub use notifications::*;
pub use preference_patch::PreferencePatch;
pub(crate) mod request_context;
pub use preferences::UiPreferences;

pub use oink_update::policy::{Command as UpdateCommand, UpdateStatus};

pub use oink_autostart::{LaunchAtLoginState, LaunchAtLoginStatus};

use std::fmt;
use std::sync::Arc;

use oink_display::layout as dl;
use oink_foundation::display::DisplayInfo;
use oink_foundation::input::{FeelProfile, PermissionState};
use oink_foundation::{DeviceId, GroupId, Point, Rect, Size};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ───────────────────────────── 标识 ─────────────────────────────

/// 界面 API 中的设备 ID：序列化为 64 位十六进制字符串（`DeviceId` 本身序列化为字节数组，用于协议）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceKey(pub DeviceId);

/// 界面 API 中的设备组 ID：序列化为 32 位十六进制字符串。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupKey(pub GroupId);

impl DeviceKey {
    /// 短指纹，如 `K7Q2-9XMF`。
    pub fn short(&self) -> String {
        self.0.short()
    }
}

impl From<DeviceId> for DeviceKey {
    fn from(id: DeviceId) -> Self {
        Self(id)
    }
}

impl From<GroupId> for GroupKey {
    fn from(id: GroupId) -> Self {
        Self(id)
    }
}

impl fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceKey({})", self.0.short())
    }
}

impl fmt::Debug for GroupKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GroupKey({})", &self.0.to_hex()[..8])
    }
}

impl Serialize for DeviceKey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_hex())
    }
}

impl<'de> Deserialize<'de> for DeviceKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_hex::<32>(&s)
            .map(|b| Self(DeviceId(b)))
            .ok_or_else(|| serde::de::Error::custom("设备 ID 应为 64 位十六进制"))
    }
}

impl Serialize for GroupKey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_hex())
    }
}

impl<'de> Deserialize<'de> for GroupKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_hex::<16>(&s)
            .map(|b| Self(GroupId(b)))
            .ok_or_else(|| serde::de::Error::custom("设备组 ID 应为 32 位十六进制"))
    }
}

fn parse_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, pair) in s.as_bytes().chunks(2).enumerate() {
        let hi = char::from(pair[0]).to_digit(16)?;
        let lo = char::from(pair[1]).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// 一块屏幕：哪台设备的哪块显示器（`display` 即 [`DisplayInfo::key`]）。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ScreenKey {
    pub device: DeviceKey,
    pub display: String,
}

impl From<&dl::ScreenId> for ScreenKey {
    fn from(id: &dl::ScreenId) -> Self {
        Self {
            device: DeviceKey(id.device),
            display: id.display.clone(),
        }
    }
}

impl From<&ScreenKey> for dl::ScreenId {
    fn from(k: &ScreenKey) -> Self {
        dl::ScreenId::new(k.device.0, k.display.clone())
    }
}

// ───────────────────────────── 错误 ─────────────────────────────

/// Typed local failures. Diagnostic text stays inside the backend; clients translate codes.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ApiError {
    /// 服务未运行或尚未就绪。
    Unavailable {
        #[serde(skip)]
        message: String,
    },
    /// 引用的设备、屏幕、请求等不存在（可能已被移除或过期）。
    NotFound {
        #[serde(skip)]
        message: String,
    },
    /// 参数不合法（例如 IP 地址格式错误、名称为空）。
    InvalidArgument {
        #[serde(skip)]
        message: String,
    },
    /// 当前状态不允许这个操作（例如已有进行中的加入流程）。
    InvalidState {
        #[serde(skip)]
        message: String,
    },
    /// 网络错误（例如手动输入的 IP 连不上）。
    Network {
        #[serde(skip)]
        message: String,
    },
    /// 本系统或本版本不支持。
    Unsupported {
        #[serde(skip)]
        message: String,
    },
    InvalidAddress,
    NotMember,
    PersistenceFailed,
    NetworkStopped,
    Internal {
        #[serde(skip)]
        message: String,
    },
}

impl ApiError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "unavailable",
            Self::NotFound { .. } => "not_found",
            Self::InvalidArgument { .. } => "invalid_argument",
            Self::InvalidState { .. } => "invalid_state",
            Self::Network { .. } => "network",
            Self::Unsupported { .. } => "unsupported",
            Self::InvalidAddress => "invalid_address",
            Self::NotMember => "not_member",
            Self::PersistenceFailed => "persistence_failed",
            Self::NetworkStopped => "network_stopped",
            Self::Internal { .. } => "internal",
        }
    }
    pub fn problem(&self) -> crate::device_cards::UserFacingProblem {
        use crate::device_cards::{UserFacingCode as Code, UserFacingProblem};
        let code = match self {
            Self::Unavailable { .. } => Code::LocalServiceUnavailable,
            Self::NotFound { .. } => Code::DeviceUnavailable,
            Self::InvalidArgument { .. } => Code::InvalidValue,
            Self::InvalidAddress => Code::InvalidAddress,
            Self::NotMember => Code::NotMember,
            Self::PersistenceFailed => Code::PersistenceFailed,
            Self::NetworkStopped => Code::NetworkStopped,
            Self::InvalidState { .. } => Code::StateConflict,
            Self::Network { .. } => Code::LinkLost,
            Self::Unsupported { .. } => Code::Unsupported,
            Self::Internal { .. } => Code::BackendFailed,
        };
        UserFacingProblem {
            code,
            unknown_code: None,
            native_code: None,
            retry_after_ms: None,
        }
    }
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidAddress => "invalid_address",
            Self::NotMember => "not_member",
            Self::PersistenceFailed => "persistence_failed",
            Self::NetworkStopped => "network_stopped",
            Self::Unavailable { message }
            | Self::NotFound { message }
            | Self::InvalidArgument { message }
            | Self::InvalidState { message }
            | Self::Network { message }
            | Self::Unsupported { message }
            | Self::Internal { message } => message,
        }
    }
}

impl Serialize for ApiError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct("ApiError", 2)?;
        value.serialize_field("code", self.code())?;
        value.serialize_field("problem", &self.problem())?;
        value.end()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for ApiError {}

pub type ApiResult<T> = Result<T, ApiError>;

// ───────────────────────── 界面框架（core） ─────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Macos,
    Windows,
    Linux,
    Other,
}

/// 一台设备的基本信息。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceSummary {
    pub id: DeviceKey,
    /// 短指纹（`DeviceId::short`），配对时供用户核对。
    pub fingerprint: String,
    pub name: String,
    pub os: Os,
    /// 如「macOS 26.0」「Windows 11 24H2」。
    pub os_version: String,
    /// 机型，如「MacBook Pro (16-inch, 2024)」；未知为 `None`。
    pub model: Option<String>,
    /// Oink 版本。
    pub app_version: String,
}

/// 服务进程的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Starting,
    Running,
    /// 用户暂停了共享：键鼠与剪贴板都只在本机。
    Paused,
    /// 在运行，但有功能不可用（例如缺少权限），详情见 `alerts`。
    Degraded,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupSummary {
    pub id: GroupKey,
    pub member_count: u32,
    pub online_count: u32,
}

/// 焦点：全局唯一的指针当前在哪台设备的哪块屏幕上。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Focus {
    pub device: DeviceKey,
    pub device_name: String,
    /// [`DisplayInfo::key`]。
    pub display: Option<String>,
    pub display_name: Option<String>,
    pub is_local: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AlertKind {
    /// 必需的系统权限未授予。
    PermissionMissing {
        permissions: Vec<PermissionItem>,
    },
    /// macOS 安全输入被某个程序开启，截获不到键盘。
    SecureInput {
        app: Option<String>,
    },
    PeerOffline {
        device: DeviceKey,
    },
    /// 出现了没有物理位置的新屏幕，建议在布局里摆放。
    NewScreen {
        screen: ScreenKey,
    },
    /// The source user triggered emergency pause. Each receiving owner records
    /// its own pause and requires an explicit user resume.
    EmergencyStop {
        device: DeviceKey,
    },
    CaptureUnavailable,
    DisplayInfoUnavailable {
        screen: ScreenKey,
    },
    Other,
    /// 链路重做：组状态文件读不出来（损坏、格式不认识、不是本机的）。后台没有启动共享，只提供 IPC；
    /// 界面据此提示，并给出「重置回初始状态」（[`AgentApi::reset_group_state`]）。
    GroupStateUnreadable,
    /// A completed native screen observation failed, once per device rather than
    /// once per cached screen. Missing peer facts are not a failure observation.
    DisplayEnumerationFailed {
        device: DeviceKey,
    },
}

/// 需要用户注意的异常（托盘图标、状态页显示）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    /// 同一个问题的 ID 保持不变，界面据此去重。
    pub id: String,
    pub level: AlertLevel,
    pub kind: AlertKind,
    pub problem: Option<crate::device_cards::UserFacingProblem>,
    pub at_ms: i64,
}

/// 界面框架需要的总体状态（托盘菜单、侧栏、顶栏）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PauseStatus {
    pub paused: bool,
    pub scope: PauseScope,
    pub revision: u64,
    pub author: Option<DeviceKey>,
    /// Current member display name; null if the author is no longer in the group.
    pub author_name: Option<String>,
    /// The local pause was triggered by an emergency event.
    pub emergency: bool,
    pub reason: crate::device_cards::PauseReason,
    /// True only after the backend has joined and removed its tap/hook thread.
    pub capture_detached: bool,
    /// Local teardown/restart or owner safety cleanup is still pending.
    pub settling: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseScope {
    Local,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Overview {
    pub this_device: DeviceSummary,
    /// Present even for a group of one. Absent only in the recovery state where the
    /// group file is unreadable and sharing has not started.
    pub group: Option<GroupSummary>,
    pub service: ServiceState,
    /// 「共享键鼠与剪贴板」总开关（托盘菜单）。
    pub sharing_enabled: bool,
    pub pause: PauseStatus,
    /// 「锁定当前焦点」：指针不会离开当前焦点设备。
    pub focus_lock: Option<DeviceSummary>,
    pub focus: Option<Focus>,
    /// 首次启动引导是否已完成；未完成时界面显示引导。
    pub onboarding_done: bool,
    pub alerts: Vec<Alert>,
}

// ─────────────────────── 设备与配对（net） ───────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerState {
    Online,
    Connecting,
    Offline {
        last_seen_ms: Option<i64>,
    },
    /// 按记住的地址连不上，组内其他成员也没有它的新地址：提示用户重新输入 IP。
    AddressStale {
        last_address: String,
    },
}

/// 连接方式。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LinkKind {
    /// 同网段自动发现后直连。
    Direct,
    /// 用户手动输入 IP 后建立的直连。
    ManualIp,
    /// 经组内另一台设备中继。
    Relay { via: DeviceKey, via_name: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkInfo {
    pub kind: LinkKind,
    /// 实际直连地址，如 `192.0.2.10:47470`；中转链路没有端到端地址。
    pub remote_address: Option<String>,
    /// 往返延迟（毫秒）。
    pub rtt_ms: Option<f64>,
    /// 丢包率（百分比）。
    pub loss_pct: f64,
    /// 剪贴板文件等大块数据走哪条连接：`separate <地址>`（独立连接）、`on_demand`（需要时建立）、
    /// `relayed via <设备>` 或 `unavailable`（原因见 bulk_unavailable）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bulk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bulk_unavailable: Option<BulkUnavailableInfo>,
}

/// Backend failure details; UI can display message and resolve hop device names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BulkUnavailableInfo {
    pub from: DeviceKey,
    pub to: DeviceKey,
    pub reason: String,
    pub port: Option<u16>,
    pub message: String,
}

/// 本组的一个成员（包括本机）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub device: DeviceSummary,
    pub is_self: bool,
    pub state: PeerState,
    pub reachable: bool,
    /// Facts have no expiry interval; retained for the UI projection and always true.
    pub facts_fresh: bool,
    /// Accepted runtime source state; offline/unknown is None, never a cached claim.
    pub keyboard_disabled: Option<bool>,
    pub mouse_mode: Option<oink_foundation::input::InputMode>,
    pub keyboard_mode: Option<oink_foundation::input::InputMode>,
    pub input_transform: Option<InputTransform>,
    pub screen_disabled: Option<bool>,
    pub screen_local_input: Option<bool>,
    pub sharing: SharingState,
    /// 与本机的连接；本机自己为 `None`。
    pub link: Option<LinkInfo>,
    /// 由谁加入本组（介绍人名称）；建组的设备为 `None`。
    pub added_by: Option<String>,
    pub added_at_ms: i64,
    pub display_count: u32,
    /// 该设备的手感档案（输入设置里显示「跟随源设备」的当前值）。
    pub feel: Option<FeelProfile>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupView {
    pub id: Option<GroupKey>,
    /// 本机排在第一位。
    pub members: Vec<Member>,
}

/// 与一台设备的共享状态，由链路就绪、本机暂停、输入流接上三者合成。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharingState {
    /// 链路没有就绪，或本机已暂停。
    NotSharing,
    /// 链路就绪、本机未暂停，输入流还没有接上。
    Establishing,
    /// 链路就绪、本机未暂停，输入流接上。
    Active,
}

impl SharingState {
    pub fn derive(link_ready: bool, paused: bool, input_attached: bool) -> Self {
        match (link_ready && !paused, input_attached) {
            (false, _) => Self::NotSharing,
            (true, false) => Self::Establishing,
            (true, true) => Self::Active,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceKind {
    Ethernet,
    Wifi,
    Vpn,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalAddress {
    pub ip: String,
    /// 网卡名，如「Wi-Fi」「以太网」。
    pub interface: String,
    pub kind: InterfaceKind,
}

/// 「本机信息」：本机的所有 IP 与监听端口。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetworkInfo {
    pub addresses: Vec<LocalAddress>,
    /// QUIC 端口，默认 47470。
    pub port: u16,
}

// Shared layout geometry.
//
// The canvas edits the logical layout, preserving gaps without displaying portals,
// walls or dead zones. Offline screens keep their positions but appear off and do
// not participate in portal calculations. This API exposes screens and positions;
// portal geometry stays inside the backend in `oink_display::layout`.

/// 屏幕物理尺寸的来源（与 `oink_display::layout::SizeSource` 对应）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeSource {
    /// 系统报告（EDID 等）且通过合理性检查。
    Reported = 0,
    /// 读不到或不合理，按分辨率估算：界面提示用户填写尺寸。
    Estimated = 1,
    /// 用户填写。
    Manual = 3,
}

/// 布局中的一块屏幕。坐标：毫米，x 向右、y 向下。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    pub key: ScreenKey,
    pub device_name: String,
    pub os: Os,
    /// Missing snapshots retain a layout-only placeholder. Its pixel dimensions
    /// are zero (unknown), never inferred from OS coordinates, which may be points.
    pub display: DisplayInfo,
    /// 物理尺寸（毫米）；手感换算按它。
    pub size: Size,
    pub size_source: SizeSource,
    /// 左上角在**逻辑布局**（保留屏幕间隙的布局）中的位置（毫米）。画布按它绘制，编辑也改它。
    pub origin: Point,
    /// 该屏的指针速度增益（默认 1.0，范围 0.1–10）。
    pub gain: f64,
    /// Extra pointer speed multiplier used only for foreign pointer sources.
    pub remote_gain: f64,
    /// 不参与布局与传送门（虚拟显示器默认隐藏）。
    pub hidden: bool,
    /// Visible/editable placement whose device cannot be a focus target.
    pub device_disabled: bool,
    /// 所属设备当前是否已连接：`true` 画成亮屏，`false` 画成熄屏（传送门计算时视为不存在）。
    pub online: bool,
}

impl Screen {
    pub fn rect(&self) -> Rect {
        Rect::new(self.origin.x, self.origin.y, self.size.w, self.size.h)
    }
}

/// A current desktop wallpaper thumbnail used as the screen canvas background.
/// 没有缩略图的屏幕不在列表里，界面按没有壁纸的样式画。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenWallpaper {
    pub key: ScreenKey,
    /// 内容的版本（内容相同则相同）：界面用它判断要不要换图。
    pub rev: String,
    /// 图片的 data URL（服务给出的是按屏幕宽高比裁剪的 JPEG，长边约 480 px）。
    pub image: String,
}

/// 壁纸可能是私人照片：调试输出里不带图片。
impl std::fmt::Debug for ScreenWallpaper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScreenWallpaper({} B)", self.image.len())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectionStatus {
    pub editing: bool,
}

/// One rigid computer's native-origin canvas placement. Scale is uniform
/// canvas units per native OS point/pixel; screens never carry editable extents.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub device: DeviceKey,
    pub origin: Point,
    pub scale: f64,
}

/// 一组屏幕及其外接矩形（布局及预览共用）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutGeometry {
    /// 全部屏幕，包括隐藏的与未连接设备的。
    pub screens: Vec<Screen>,
    /// 未隐藏屏幕的外接矩形。
    pub bounds: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutSource {
    /// 自动排列的初始布局。
    Default,
    /// 用户手动摆放。
    Manual,
}

/// 已保存的布局（组内同步）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// 每次保存加一；组内同步时用来判断新旧。
    pub revision: u64,
    pub source: LayoutSource,
    pub geometry: LayoutGeometry,
}

/// [`layout_geometry`] 需要的每块屏幕的附加信息（布局本身不含设备名与完整的显示器信息）。
#[derive(Clone, Debug)]
pub struct ScreenMeta {
    pub device_name: String,
    pub os: Os,
    /// 该屏最近一次上报的显示器信息；`None`（例如离线设备的信息没保存）时由布局中的放置信息补出。
    pub display: Option<DisplayInfo>,
}

impl From<dl::SizeSource> for SizeSource {
    fn from(s: dl::SizeSource) -> Self {
        match s {
            dl::SizeSource::Reported => Self::Reported,
            dl::SizeSource::Estimated => Self::Estimated,
            dl::SizeSource::Manual => Self::Manual,
        }
    }
}

/// 把逻辑布局（`oink_display::layout::Layout`）转成界面的 [`LayoutGeometry`]。
///
/// Drawing data comes from `layout.view(online)`, including screen geometry and availability.
/// 尺寸来源与增益取自对应的 `ScreenPlacement`；`meta` 给出设备名、系统与显示器信息，返回 `None` 的屏幕跳过。
/// 服务进程与假数据都用它，保证界面看到的是同一套规整后的布局。
pub fn layout_geometry(
    layout: &dl::Layout,
    owners: &std::collections::BTreeMap<DeviceId, dl::OwnerLayoutPreferences>,
    online: impl Fn(&DeviceId) -> bool,
    meta: impl Fn(&dl::ScreenId) -> Option<ScreenMeta>,
) -> LayoutGeometry {
    let view = layout.view(owners, online);
    let screens: Vec<Screen> = view
        .screens
        .iter()
        .filter_map(|v| {
            let m = meta(&v.id)?;
            let placement = layout.screen(&v.id)?;
            let mut display = m.display.unwrap_or_else(|| {
                tracing::debug!(device = %v.id.device, display = %v.id.display,
                    "display snapshot unavailable; using layout-only placeholder with unknown pixels");
                DisplayInfo {
                    key: v.id.display.clone(),
                    name: v.name.clone(),
                    bounds: placement.local,
                    pixel_width: 0,
                    pixel_height: 0,
                    reference_extent: None,
                    rotation_deg: 0,
                    is_primary: v.primary,
                    is_builtin: v.builtin,
                    is_virtual: false,
                }
            });
            display.bounds = placement.local;
            display.name = v.name.clone();
            display.is_primary = v.primary;
            display.is_builtin = v.builtin;
            Some(Screen {
                key: ScreenKey::from(&v.id),
                device_name: m.device_name,
                os: m.os,
                display,
                size: Size::new(v.rect.w, v.rect.h),
                size_source: placement.size_source.into(),
                origin: Point::new(v.rect.x, v.rect.y),
                gain: owners
                    .get(&v.id.device)
                    .cloned()
                    .unwrap_or_default()
                    .screen(placement)
                    .gain,
                remote_gain: owners
                    .get(&v.id.device)
                    .cloned()
                    .unwrap_or_default()
                    .screen(placement)
                    .remote_gain,
                hidden: !v.enabled,
                device_disabled: owners.get(&v.id.device).is_some_and(|owner| !owner.screen_shared),
                online: v.online,
            })
        })
        .collect();
    let visible: Vec<Rect> = screens.iter().filter(|s| !s.hidden).map(Screen::rect).collect();
    let bounds = bounds_of_rects(&visible).or(view.bounds).unwrap_or_default();
    LayoutGeometry { screens, bounds }
}

fn bounds_of_rects(rects: &[Rect]) -> Option<Rect> {
    let first = rects.first()?;
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.right(), first.bottom());
    for r in &rects[1..] {
        x0 = x0.min(r.x);
        y0 = y0.min(r.y);
        x1 = x1.max(r.right());
        y1 = y1.max(r.bottom());
    }
    Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
}

// ───────────────────────── 输入设置（core） ─────────────────────────
//
// The service converts these local fields to and from routing settings.

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputTransform {
    pub macos: oink_foundation::input_transform::TargetTransform,
    pub windows: oink_foundation::input_transform::TargetTransform,
    pub overrides: Vec<InputTargetOverride>,
    /// Read-only per-entry self limitations; ignored when sending an edit.
    #[serde(default, skip_deserializing)]
    pub local_exclusions: Vec<InputLocalExclusion>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputLocalExclusion {
    pub scope: String,
    pub target: Option<DeviceKey>,
    pub mapping: oink_foundation::input_transform::KeyMapping,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputTargetOverride {
    pub target: DeviceKey,
    pub reverse_wheel: Option<bool>,
    pub keyboard: Option<oink_foundation::input_transform::KeyboardMapping>,
}
impl InputTransform {
    pub fn initial(os: &str) -> Self {
        oink_foundation::input_transform::InputTransform::initial(os).into()
    }
    pub fn with_local_exclusions(mut self, source: DeviceId, os: &str) -> Self {
        self.local_exclusions.clear();
        if os == "macos" {
            let mut add = |scope: &str,
                           target: Option<DeviceKey>,
                           keyboard: &oink_foundation::input_transform::KeyboardMapping| {
                for mapping in &keyboard.table {
                    if let Some(reason) = oink_foundation::input_transform::mac_local_mapping_hint(*mapping) {
                        self.local_exclusions.push(InputLocalExclusion {
                            scope: scope.into(),
                            target,
                            mapping: *mapping,
                            reason: reason.into(),
                        });
                    }
                }
            };
            add("macos", None, &self.macos.keyboard);
            for item in &self.overrides {
                if item.target.0 == source
                    && let Some(keyboard) = &item.keyboard
                {
                    add("device", Some(item.target), keyboard);
                }
            }
        }
        self
    }
}
impl From<oink_foundation::input_transform::InputTransform> for InputTransform {
    fn from(value: oink_foundation::input_transform::InputTransform) -> Self {
        Self {
            local_exclusions: Vec::new(),
            macos: value.macos,
            windows: value.windows,
            overrides: value
                .overrides
                .into_iter()
                .map(|o| InputTargetOverride {
                    target: o.target.into(),
                    reverse_wheel: o.reverse_wheel,
                    keyboard: o.keyboard,
                })
                .collect(),
        }
    }
}
impl From<InputTransform> for oink_foundation::input_transform::InputTransform {
    fn from(value: InputTransform) -> Self {
        Self {
            macos: value.macos,
            windows: value.windows,
            overrides: value
                .overrides
                .into_iter()
                .map(|o| oink_foundation::input_transform::TargetOverride {
                    target: o.target.0,
                    reverse_wheel: o.reverse_wheel,
                    keyboard: o.keyboard,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollSettings {
    pub speed: f64,
}

/// 本机作为源时换算位移用的参考屏（对应 `fixed_reference_display`）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PointerReference {
    /// 指针离开本机时所在的那块屏幕（默认）。
    LastScreen,
    /// 固定为**本机**的某块屏幕（[`DisplayInfo::key`]）。
    Fixed { display: String },
}

/// 对应 `oink_display::layout::SpeedMode`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerScaling {
    /// 按物理距离换算：同样的手部动作在任何屏幕上移动同样的物理距离（默认）。
    Physical,
    /// 按屏幕比例换算：同样的动作走过屏幕对角线的同样比例。
    Proportional,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerSettings {
    pub reference: PointerReference,
    pub scaling: PointerScaling,
    /// 按住鼠标按键时是否允许指针移到其他设备（对应 `button_handoff`；默认 `false`：拖动窗口、选择文字时不会意外跳走）。
    pub drag_across_devices: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyAction {
    /// 锁定当前焦点：指针不再离开当前焦点设备。
    LockFocus,
    /// 把指针召回本机主屏。
    ReturnPointerHome,
    /// 暂停 / 恢复共享。
    ToggleSharing,
    /// Pause every currently admitted reachable owner through one emergency event.
    PauseAll,
    /// 打开 Oink 窗口。
    OpenWindow,
    /// 锁定当前焦点机并发送原始相对位移，再按一次恢复普通指针。
    ToggleGameMode,
}

impl HotkeyAction {
    /// 全部动作（界面列表的顺序）。
    pub const ALL: &[HotkeyAction] = &[
        HotkeyAction::LockFocus,
        HotkeyAction::ReturnPointerHome,
        HotkeyAction::ToggleSharing,
        HotkeyAction::PauseAll,
        HotkeyAction::OpenWindow,
        HotkeyAction::ToggleGameMode,
    ];

    /// 序列化时的名字（`"lock_focus"` 等），也是路由 `StatusEvent::Hotkey::action` 的写法。
    pub fn as_str(self) -> &'static str {
        match self {
            HotkeyAction::LockFocus => "lock_focus",
            HotkeyAction::ReturnPointerHome => "return_pointer_home",
            HotkeyAction::ToggleSharing => "toggle_sharing",
            HotkeyAction::PauseAll => "pause_all",
            HotkeyAction::OpenWindow => "open_window",
            HotkeyAction::ToggleGameMode => "toggle_game_mode",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|a| a.as_str() == s)
    }
}

/// 全局快捷键。`keys` 为按键名（修饰键 `Ctrl` `Alt` `Shift` `Meta`，其余为 `KeyboardEvent.code` 去掉前缀，
/// 如 `L`、`F12`、`Backquote`）；`Meta` 在 Mac 上是 Cmd，在 Windows 上是 Win 键。
///
/// Recognized on the physical keyboard owner regardless of focus; consumed locally.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hotkey {
    pub action: HotkeyAction,
    pub keys: Vec<String>,
    pub enabled: bool,
}

pub use oink_display::layout::EventFrequency;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeySupport {
    pub macos: Vec<oink_foundation::input::KeyCode>,
    pub windows: Vec<oink_foundation::input::KeyCode>,
}
impl Default for KeySupport {
    fn default() -> Self {
        Self {
            macos: oink_input::supported_key_codes("macos"),
            windows: oink_input::supported_key_codes("windows"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputSettings {
    /// Observed native Caps Lock input-source preference; never writable by clients.
    // Replies and subscription events are deserialized by the desktop bridge too.
    // The settings owner ignores this field on edits; skipping it here loses native observations.
    #[serde(default)]
    pub mac_caps_switches_input_source: Option<bool>,
    /// Read-only native support, projected from the actual platform tables.
    #[serde(default, skip_deserializing)]
    pub key_support: KeySupport,
    /// Local source motion rate.
    #[serde(default)]
    pub event_frequency: EventFrequency,
    pub accept_contacts: bool,
    /// Receiver-local Windows scroll feel; retained while contact acceptance is off.
    #[serde(default = "default_true")]
    pub mac_scroll_feel: bool,
    /// Windows source-local preference. Never replicated to peers.
    #[serde(default)]
    pub forward_injected_input: bool,
    /// Read-only live native injector availability; absent means unavailable.
    #[serde(default)]
    pub contacts_injection_available: bool,
    pub emergency_hold_ms: u32,
    pub device_gain: f64,
    pub scroll: ScrollSettings,
    pub pointer: PointerSettings,
    pub hotkeys: Vec<Hotkey>,
    /// 触摸板手势转发给正在操作的那台（`RoutingSettings::forward_gestures`）：目标是 Mac 时重放，
    /// 是 Windows 时优先原生触点（由组设置控制），否则映射为快捷键。关闭时控制其他设备期间的手势被忽略。
    pub gestures_enabled: bool,
    #[serde(default = "default_return_pointer_on_display_sleep")]
    pub return_pointer_on_display_sleep: bool,
    /// Respond to group user activity by waking displays.
    #[serde(default = "default_return_pointer_on_display_sleep")]
    pub wake_displays_on_input: bool,
    /// Group-wide circle gesture / trail switch, persisted and synchronized with LayoutSettings.
    #[serde(default = "default_true")]
    pub cursor_trail: bool,
    /// Group-wide ribbon-point lifetime (100–60000 ms), synchronized with LayoutSettings.
    #[serde(
        default = "oink_display::layout::default_cursor_trail_duration_ms",
        deserialize_with = "oink_display::layout::deserialize_cursor_trail_duration_ms"
    )]
    pub cursor_trail_duration_ms: u32,
    /// Group setting stored and shared with the layout.
    pub gap_travel: bool,
}

fn default_return_pointer_on_display_sleep() -> bool {
    true
}

fn default_true() -> bool {
    true
}

// ───────────────── 剪贴板设置（net：剪贴板同步；core 保存） ─────────────────

/// Per-device disk cache snapshots; no content names or paths.
pub use oink_clipboard::cache::{CacheClear, CacheUsage};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheDevice {
    pub device: DeviceKey,
    pub usage: CacheUsage,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheDeviceClear {
    pub device: DeviceKey,
    pub result: CacheClear,
}

/// Five local clipboard format filters; the device owner controls the master switch.
pub use oink_clipboard::policy::FormatFilters as ClipboardSettings;
pub use oink_clipboard::received::{Cleanup as ReceivedFilesCleanup, Usage as ReceivedFilesUsage};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipKind {
    Text,
    Html,
    Rtf,
    Image,
    Files,
    Custom,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClipOutcome {
    Synced,
    /// 没有同步：`reason` 为给人看的原因（超过上限、该类型已关闭）。
    Skipped {
        reason: String,
    },
    Failed {
        message: String,
    },
}

/// 最近的剪贴板同步记录。**不含内容**，只有类型与大小（隐私）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipActivity {
    pub at_ms: i64,
    pub from_name: String,
    pub kinds: Vec<ClipKind>,
    pub bytes: u64,
    pub outcome: ClipOutcome,
}

/// Current incoming file offer/transfer. Contains progress, never file contents.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileTransferStatus {
    pub id: String,
    pub from_name: String,
    pub done: u64,
    pub total: u64,
    pub files_done: usize,
    pub files: usize,
    pub phase: FileTransferPhase,
    pub message: Option<String>,
}

/// Local macOS File Provider enablement, independent of clipboard transfer progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileProviderState {
    Unsupported,
    Checking,
    Unregistered,
    NotEnabled,
    Available,
    Disconnected,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProviderStatus {
    #[serde(default)]
    pub reason: Option<oink_clipboard::FileProviderReason>,
    pub state: FileProviderState,
    /// A remote clipboard/drag has actually attempted to use the provider this session.
    pub needed: bool,
    /// An explicit or automatic request to open Finder is queued or running.
    pub opening: bool,
    pub detail: Option<String>,
}

impl Default for FileProviderStatus {
    fn default() -> Self {
        Self {
            state: FileProviderState::Unsupported,
            reason: None,
            needed: false,
            opening: false,
            detail: None,
        }
    }
}

/// Device-local experiment setting and its nonblocking runtime snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProviderPlaceholderStatus {
    pub enabled: bool,
    pub supported: bool,
    /// Live inventory only; per-drag usage and timing belong to diagnostics.
    pub stock: Option<FileProviderPlaceholderStock>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileProviderPlaceholderState {
    Disabled,
    Preparing,
    Ready,
    Empty,
    CleanupPending,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProviderPlaceholderStock {
    pub state: FileProviderPlaceholderState,
    pub ready_parents: u32,
    pub files_per_parent: u32,
    pub folders_per_parent: u32,
    pub preparing: bool,
    pub last_background_failure: Option<oink_clipboard::FileProviderReason>,
    pub cleanup_pending: u32,
}

impl From<oink_clipboard::provider_preparation::PlaceholderPoolStatus> for FileProviderPlaceholderStock {
    fn from(stock: oink_clipboard::provider_preparation::PlaceholderPoolStatus) -> Self {
        // Outstanding retirement is visible even after off. Ready inventory
        // remains usable while another parent is being prepared.
        let state = if stock.cleanup_pending != 0 {
            FileProviderPlaceholderState::CleanupPending
        } else if !stock.enabled {
            FileProviderPlaceholderState::Disabled
        } else if stock.ready_parents != 0 {
            FileProviderPlaceholderState::Ready
        } else if stock.preparing {
            FileProviderPlaceholderState::Preparing
        } else {
            FileProviderPlaceholderState::Empty
        };
        Self {
            state,
            ready_parents: stock.ready_parents,
            files_per_parent: stock.files_per_parent,
            folders_per_parent: stock.folders_per_parent,
            preparing: stock.preparing,
            last_background_failure: stock.last_background_failure,
            cleanup_pending: stock.cleanup_pending,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTransferPhase {
    Pending,
    Downloading,
    Ready,
    Failed,
    Cancelled,
}

// ─────────────────────── 状态与诊断（core） ───────────────────────

/// Per-client live measurements. State changes still reach every subscriber.
/// The UI enables each field only while a visible view displays it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveViews {
    pub stats: bool,
    pub group_quality: bool,
    pub audio_quality: bool,
    pub diagnostics: bool,
}

impl LiveViews {
    pub const ALL: Self = Self {
        stats: true,
        group_quality: true,
        audio_quality: true,
        diagnostics: true,
    };
}

/// A process-monotonic timestamp and the existing link RTT, in milliseconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QualitySample {
    pub at_ms: u64,
    pub rtt_ms: Option<f64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerQuality {
    pub device: DeviceKey,
    pub name: String,
    pub path: LinkKind,
    pub samples: Vec<QualitySample>,
    pub current_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p90_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub max_ms: Option<f64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnectionQuality {
    pub revision: u64,
    pub sampled_at_ms: u64,
    pub peers: Vec<PeerQuality>,
}

/// Local receive history; never part of audio wire messages or Facts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioQualitySample {
    pub at_ms: u64,
    pub buffer_ms: Option<f64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioSourceQuality {
    pub device: DeviceKey,
    pub name: String,
    pub samples: Vec<AudioQualitySample>,
    pub buffer_avg_ms: Option<f64>,
    pub bitrate_kbps: f64,
    pub interruptions: u64,
    pub concealed_ms: f64,
    pub late_packets: u64,
    pub glitches: Vec<oink_audio::sink::DropoutBucket>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioQuality {
    pub revision: u64,
    pub sampled_at_ms: u64,
    pub sources: Vec<AudioSourceQuality>,
    pub output: Option<AudioOutputQuality>,
}

/// Shared native output FIFO; never attributed to any particular source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioOutputQuality {
    pub interruptions: u64,
    pub missing_ms: f64,
    pub glitches: Vec<oink_audio::sink::DropoutBucket>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub device: DeviceKey,
    pub name: String,
    pub os: Os,
    pub state: PeerState,
    pub reachable: bool,
    /// Facts have no expiry interval; retained for the UI projection and always true.
    pub facts_fresh: bool,
    /// The target's advertisement on its current input stream, not an OS guess.
    pub touchpad_contacts: Option<u8>,
    pub contacts_supported: bool,
    pub contacts_path: PeerContactsPath,
    pub sharing: SharingState,
    pub link: Option<LinkInfo>,
    /// 最近 60 次往返延迟采样（毫秒，每秒一次），画走势图用。
    pub rtt_history_ms: Vec<f64>,
}

/// Selected target path assuming gestures are forwarded; source capture, focus,
/// pause and the local gesture-forwarding switch still gate actual delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerContactsPath {
    Contacts,
    ShortcutsDisabledBySetting,
    ShortcutsTargetUnsupported,
}

impl PeerContactsPath {
    pub fn derive(enabled: bool, capacity: Option<u8>) -> Self {
        if !enabled {
            Self::ShortcutsDisabledBySetting
        } else if capacity == Some(5) {
            Self::Contacts
        } else {
            Self::ShortcutsTargetUnsupported
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusView {
    #[serde(default)]
    pub network: oink_network::UdpSendStatus,
    /// Issues from this local startup read; contains no setting values.
    #[serde(default)]
    pub settings_issues: Vec<crate::config::SettingsIssue>,
    /// Actual backend job dependency; older peers may omit this field.
    #[serde(default)]
    pub backend_lifetime: String,
    pub backend_uiaccess: Option<bool>,
    pub contact_injection: oink_input::ContactInjectionStatus,
    pub accept_contacts: bool,
    pub service: ServiceState,
    pub version: String,
    pub uptime_s: u64,
    pub focus: Option<Focus>,
    /// 组内其他设备。
    pub peers: Vec<PeerStatus>,
    pub alerts: Vec<Alert>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// 一行日志。日志不含按键与剪贴板内容。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    /// 单调递增，`logs(after)` 用它增量拉取。
    pub seq: u64,
    pub at_ms: i64,
    pub level: LogLevel,
    /// `tracing` 的 target，如 `oink_network::discovery`。
    pub target: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticsBundle {
    /// 本机诊断 ZIP 路径（脱敏记录与环境；不含键码、设备名或剪贴板内容）。
    pub path: String,
    pub bytes: u64,
    pub cutoff_complete: Option<bool>,
    pub included_device_count: Option<usize>,
    pub missing_device_count: Option<usize>,
}

/// Display-only enrichment at the local API boundary. Never part of recorder storage or wire data.
pub type DiagnosticRecorderStatus = crate::diagnostics::RecorderStatus;

/// Audio-owner samples; no settings, lifecycle, permission or error fields.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioStatistics {
    pub capture_levels: Option<oink_audio::CaptureLevels>,
    pub capture_diagnostics: Option<serde_json::Value>,
    pub playback_latency_ms: Option<f64>,
    pub sent: u64,
    pub send_dropped: u64,
    pub sources: Vec<(oink_foundation::DeviceId, oink_audio::dsp::Stats)>,
    pub network_rtt_ms: Option<f64>,
    pub estimated_latency_ms: Option<f64>,
    pub device_states: Vec<DeviceAudioStatistics>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceAudioStatistics {
    pub device: oink_foundation::DeviceId,
    pub age_ms: u64,
    pub sent: u64,
    pub send_dropped: u64,
    pub outputs: Vec<oink_audio::OutputLatency>,
    pub receivers: Vec<oink_audio::ReceiverReport>,
}
/// Recorder-worker samples; control transitions remain DiagnosticsStatus.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticStatistics {
    pub bytes: u64,
    pub queued: usize,
    pub dropped: u64,
    pub errors: u64,
    pub segments: usize,
    pub oldest_utc_us: Option<i64>,
    pub newest_utc_us: Option<i64>,
    pub corrupt_segments: u64,
    pub discarded_segments: crate::diagnostics::DiscardCounts,
    pub evicted_segments: u64,
}

// ─────────────────────── 权限与引导（core） ───────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    /// macOS「辅助功能」：注入、屏蔽键鼠。
    Accessibility,
    /// macOS「输入监控」：截获键盘。
    InputMonitoring,
    /// macOS 15+ Local Network privacy: discovery and connections.
    LocalNetwork,
    /// Optional macOS system-audio capture permission.
    AudioCapture,
    /// Windows 防火墙入站规则（默认 UDP 47470/47471：交互连接/发现；TCP 47472：Low 数据流）。
    Firewall,
    /// Windows 系统服务：锁屏、管理员授权弹窗也能被远端键鼠操作（可选）。
    SystemService,
}

impl PermissionKind {
    /// macOS「系统设置」中对应面板的 URL（`open` 命令打开）。
    pub fn macos_settings_url(self) -> Option<&'static str> {
        match self {
            Self::Accessibility => {
                Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            }
            Self::InputMonitoring => {
                Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent")
            }
            // Local Network has no working reveal anchor on macOS 26. Apple's
            // guide opens this parent pane and instructs the user to select Local
            // Network; the Permissions row shows that navigation explicitly.
            Self::LocalNetwork => Some("x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension"),
            Self::AudioCapture => Some("x-apple.systempreferences:com.apple.preference.security?Privacy_AudioCapture"),
            Self::Firewall | Self::SystemService => None,
        }
    }

    /// Windows 设置中对应页面的 URI（`start` 打开）。
    pub fn windows_settings_uri(self) -> Option<&'static str> {
        match self {
            Self::Firewall => Some("windowsdefender://network"),
            Self::SystemService => Some("ms-settings:appsfeatures"),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionItem {
    pub kind: PermissionKind,
    pub state: PermissionState,
    /// 必需（缺少时共享不可用）还是可选。
    pub required: bool,
}

/// 一块屏幕上显示的内容。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OverlayContent {
    /// 识别屏幕：大字显示「设备名 · 屏幕名」。
    Identify,
}

/// 一块本机屏幕上的窗口。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OverlayScreen {
    /// 本机 `DisplayInfo::key`。
    pub display: String,
    pub device_name: String,
    pub display_name: String,
    pub content: OverlayContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayKind {
    Identify,
}

/// Short-lived identification windows on this device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Overlay {
    pub id: u64,
    pub kind: OverlayKind,
    pub screens: Vec<OverlayScreen>,
    /// Wall-clock expiry; the backend dismisses the windows after three seconds.
    pub expires_at_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OverlayShown {
    pub display: String,
    /// Actual pixel dimensions of the rendered window.
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// 显示失败的原因（找不到这块屏幕、窗口创建失败）；成功为 `None`。
    pub error: Option<String>,
}

// ───────────────────────────── 事件 ─────────────────────────────

/// Game mode of the currently focused device. Commands to a remote holder are asynchronous.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GameModeStatus {
    pub holder: DeviceKey,
    pub epoch: u64,
    pub mode: oink_routing::GameMode,
    pub detected: bool,
    pub active: bool,
    /// Current focus device: true = cause readable, false = visibility-only fallback,
    /// None = not yet sampled. Informational: false does not disable automatic capture.
    pub cursor_hiding_cause_available: Option<bool>,
}
impl From<oink_routing::GameModeStatus> for GameModeStatus {
    fn from(s: oink_routing::GameModeStatus) -> Self {
        Self {
            holder: s.holder.into(),
            epoch: s.epoch,
            mode: s.mode,
            detected: s.detected,
            active: s.active,
            cursor_hiding_cause_available: s.cursor_hiding_cause_available,
        }
    }
}

// ───────────────────────────── 瞄准小游戏 ─────────────────────────────

/// Current phase of the aiming exercise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AimPhase {
    /// 没有进行中的游戏。
    #[default]
    Idle,
    /// 开局倒计时。
    Countdown,
    Running,
    /// 时间到，叠加层上显示着结果（几秒后或点击 / Esc 后收起）。
    Finished,
}

/// 瞄准小游戏的状态。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AimStatus {
    /// 本机能否显示全屏叠加层（平台实现）。`false` 时不能开始。
    pub available: bool,
    pub phase: AimPhase,
    /// 这一局是本机发起的（本机计分、计时）；其它设备发起的一局里本机只显示叠加层。
    pub host: bool,
    /// 进行中：这一局是在暂停共享时开的（各设备用自己的原生输入）。空闲时：现在开始会是这种局
    /// （即本机此刻处于暂停共享）。
    pub native_input: bool,
    /// 当前阶段剩余的毫秒数（发出事件的时刻；界面按本机时钟倒数）。
    pub remaining_ms: u32,
    pub score: u32,
    /// 计入命中率的点击次数。
    pub clicks: u32,
    /// 参与的设备数（含本机）；本机不是发起方时为 0。
    pub devices: u32,
    /// 标靶直径（视平面毫米），所有屏幕上相同。
    pub target_diameter: f64,
    /// 一局的时长。
    pub duration_ms: u32,
    /// 本机发起的最近几局的结果（新的在前，最多 10 局；不保存，服务重启后清空）。
    pub results: Vec<AimResult>,
}

/// 一局的结果。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AimResult {
    /// 这一局是在暂停共享时开的：每台设备用自己的原生输入（基线）。否则输入经 Oink 转发。
    pub native_input: bool,
    /// 进行中共享的暂停状态变过（这一局不适合用来对比）。
    pub mode_changed: bool,
    pub score: u32,
    pub clicks: u32,
    /// 命中率（0–1）；没有点击时为 `None`。
    pub accuracy: Option<f64>,
    pub hits_per_sec: f64,
    /// 平均命中间隔；没有命中时为 `None`。
    pub avg_hit_interval_ms: Option<f64>,
    /// 实际进行的时长（提前结束时小于一局的时长）。
    pub played_ms: u32,
    /// 是否打满了时间。
    pub completed: bool,
    /// 参与的设备数（含本机）。
    pub devices: u32,
    pub ended_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // 只用于传递与序列化，装箱没有收益
pub enum ApiEvent {
    LayoutProfiles(crate::layout_profiles::types::LayoutProfilesState),
    LayoutProfileOperation(crate::layout_profiles::types::OperationEvent),
    ConnectionQuality(ConnectionQuality),
    AudioQuality(AudioQuality),
    NearbyChanged {
        revision: crate::device_cards::DecimalU64,
        upserts: Vec<pairing::NearbyRow>,
        removed: Vec<DeviceKey>,
    },
    NearbySnapshot(pairing::NearbySnapshot),
    NearbyRefreshFinished {
        operation_id: String,
        result: pairing::TypedResult,
    },
    AddressQueryFinished {
        operation_id: String,
        generation: crate::device_cards::DecimalU64,
        result: pairing::AddressQueryResult,
    },
    PairingChanged {
        revision: crate::device_cards::DecimalU64,
        view: pairing::PairView,
    },
    PairingRequestsChanged {
        revision: crate::device_cards::DecimalU64,
        requests: Vec<pairing::PendingRequest>,
    },
    GroupMigrationChanged(pairing::GroupMigrationStatus),
    PairingFailed {
        event_id: crate::device_cards::DecimalU64,
        pair_id: pairing::PairId,
        peer: pairing::PairEndpoint,
        problem: pairing::PairProblem,
    },
    PairingCompleted {
        event_id: crate::device_cards::DecimalU64,
        pair_id: pairing::PairId,
        peer: pairing::PeerView,
        merge_id: String,
        target_group: pairing::GroupRef,
        imported_devices: Vec<DeviceKey>,
        undo: pairing::UndoHandle,
        reconciled: bool,
    },
    MembershipEdited {
        operation_id: String,
        result: pairing::TypedResult,
    },

    DeviceCards(crate::device_cards::DeviceCardsView),
    DeviceControlCompleted(crate::device_cards::DeviceControlCompleted),
    BlockedDevices {
        snapshot: crate::device_cards::BlockedDevicesSnapshot,
    },
    UiPreferences(UiPreferences),
    /// Local update drain: exit only the UI client, without stopping its backend.
    ExitForUpdate,
    UpdateStatusChanged(UpdateStatus),
    /// Ephemeral local notice only; never replayed in a state query.
    DndExportFailed(oink_foundation::dnd::ExportFailure),
    /// One local, non-replayed preparation failure. Contains no file metadata.
    DndPreparationFailed(String),
    DiagnosticsStatus(DiagnosticRecorderStatus),
    ReportStatusChanged(crate::telemetry::ReportStatus),
    /// Source-owned measurements, published at most once per second / three seconds.
    AudioStats(AudioStatistics),
    DiagnosticsStats(DiagnosticStatistics),
    LaunchAtLogin(LaunchAtLoginStatus),
    Audio(oink_audio::Status),
    GameMode(GameModeStatus),
    /// core
    Overview(Overview),
    Pause(PauseStatus),
    /// core：焦点移动（比 `Overview` 频繁）。
    Focus(Option<Focus>),
    /// net：成员、状态、连接方式变化。
    Group(GroupView),
    InputTransformEdited {
        device: DeviceKey,
        error: Option<String>,
    },
    Layout(Layout),
    LayoutProjection(ProjectionStatus),
    /// core
    InputSettings(InputSettings),
    /// core
    CacheUsage(Vec<CacheDevice>),
    ReceivedFilesUsage(ReceivedFilesUsage),
    ClipboardSettings(ClipboardSettings),
    /// net
    ClipActivity(ClipActivity),
    FileTransfer(Option<FileTransferStatus>),
    FileProviderStatus(FileProviderStatus),
    /// core
    Permissions(Vec<PermissionItem>),
    /// core：每秒一次的连接质量。
    Stats(Vec<PeerStatus>),
    /// core
    Log(LogLine),
    /// Emergency pause has taken effect. A local trigger brings the main window
    /// forward; remote triggers show a status-item notice. Later state is in
    /// `Overview` through `AlertKind::EmergencyStop`.
    EmergencyStop(EmergencyStop),
    /// The local open-window shortcut requests that the UI bring its main window forward.
    ShowWindow,
    /// Local status-item click; coordinates never travel to peers.
    TogglePanel(crate::backend::PanelAnchor),
    /// Windows backend asks the tray/UI process to explicitly reopen both processes.
    ReopenOink,
    /// core：瞄准小游戏的状态（阶段、得分变化时；进行中每 250 ms 一次）。
    Aim(AimStatus),
    /// core：某块屏幕的壁纸缩略图变了（本机的或其他设备的）；界面重新调用 `wallpapers`。
    /// 事件本身不带图片。
    Wallpapers,
    /// Local input measurements, from the same ordinary statistics clock.
    InputStats(oink_input::timing::Sample),
}

/// 紧急暂停事件。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmergencyStop {
    /// 触发的设备。
    pub device: DeviceKey,
    pub device_name: String,
    /// 本机触发。
    pub local: bool,
    pub at_ms: i64,
}

/// 事件回调。可能在任意线程调用；实现方不得在持有内部锁时调用它。
pub type EventSink = Arc<dyn Fn(&ApiEvent) + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubscriptionId(pub u64);

// ───────────────────────────── trait ─────────────────────────────

/// 界面调用服务进程的接口。实现必须线程安全；命令可能阻塞（例如 `connect_by_ip` 等待网络），
/// 界面在后台线程调用。
pub trait AgentApi: Send + Sync {
    fn layout_profile_command(&self, command: crate::layout_profiles::types::Command) -> ApiResult<serde_json::Value>;

    fn device_cards(&self) -> ApiResult<crate::device_cards::DeviceCardsView>;
    fn invoke_device_card_action(
        &self,
        request_id: String,
        token: String,
    ) -> ApiResult<crate::device_cards::DeviceControlReply>;
    fn set_forwarding_controls(
        &self,
        request_id: String,
        version_token: String,
        volume_percent: u16,
        muted: bool,
    ) -> ApiResult<crate::device_cards::DeviceControlReply>;
    /// Opt-in for live dragging. Missing/false keeps the released synchronous
    /// durability contract; implementations without this extension remain safe.
    fn set_forwarding_controls_with_persistence(
        &self,
        request_id: String,
        version_token: String,
        volume_percent: u16,
        muted: bool,
        _defer_persistence: bool,
    ) -> ApiResult<crate::device_cards::DeviceControlReply> {
        self.set_forwarding_controls(request_id, version_token, volume_percent, muted)
    }
    fn set_exit_volume(
        &self,
        request_id: String,
        version_token: String,
        volume_percent: u8,
        muted: bool,
    ) -> ApiResult<crate::device_cards::DeviceControlReply>;
    fn set_local_paused(&self, request_id: String, paused: bool) -> ApiResult<crate::device_cards::DeviceControlReply>;
    fn set_device_paused(
        &self,
        request_id: String,
        token: String,
        paused: bool,
    ) -> ApiResult<crate::device_cards::DeviceControlReply>;
    fn pause_all(&self, request_id: String) -> ApiResult<crate::device_cards::PauseAllAccepted>;
    fn member_addresses(&self, device: DeviceKey) -> ApiResult<Vec<String>>;
    fn set_member_address(&self, device: DeviceKey, address: String) -> ApiResult<()>;
    fn nearby(&self) -> ApiResult<pairing::NearbySnapshot>;
    fn refresh_nearby(&self, operation_id: String) -> ApiResult<pairing::PairReply<pairing::Accepted>>;
    fn query_pairing_address(
        &self,
        operation_id: String,
        generation: crate::device_cards::DecimalU64,
        ip: String,
        port: Option<u16>,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>>;
    fn release_pairing_query(&self, query_id: String, generation: crate::device_cards::DecimalU64) -> ApiResult<()>;
    fn start_pairing(
        &self,
        operation_id: String,
        target: pairing::PairTarget,
    ) -> ApiResult<pairing::PairReply<pairing::Started>>;
    fn pairings(&self) -> ApiResult<pairing::PairingSnapshot>;
    /// Ephemeral local admission policy while the welcome flow connects a peer.
    fn set_onboarding_connect(&self, active: bool) -> ApiResult<()>;
    fn group_migration(&self) -> ApiResult<Option<pairing::GroupMigrationStatus>>;
    fn accept_pairing(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>>;
    fn decline_pairing(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>>;
    fn cancel_pairing(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    ) -> ApiResult<pairing::PairReply<pairing::CancelResult>>;
    fn pairing_code(&self, pair_id: String) -> ApiResult<pairing::CodeView>;
    fn submit_pairing_code(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
        attempt: u8,
        code: String,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>>;
    fn undo_pairing(
        &self,
        operation_id: String,
        handle: pairing::UndoHandle,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>>;
    fn subscribe_pairing_events(
        &self,
        after_event_id: crate::device_cards::DecimalU64,
    ) -> ApiResult<Vec<pairing::PairingEvent>>;
    fn ack_pairing_event(&self, event_id: crate::device_cards::DecimalU64) -> ApiResult<()>;
    fn blocked_devices(&self) -> ApiResult<crate::device_cards::BlockedDevicesSnapshot>;
    fn block(&self, operation_id: String, device: DeviceKey) -> ApiResult<crate::device_cards::BlockReply>;
    fn unblock(&self, operation_id: String, device: DeviceKey) -> ApiResult<crate::device_cards::BlockReply>;
    fn notify(&self, notification: Notification) -> ApiResult<Option<NotificationReceipt>>;
    fn ui_preferences(&self) -> ApiResult<UiPreferences>;
    fn set_ui_preferences(&self, settings: PreferencePatch<UiPreferences>) -> ApiResult<UiPreferences>;
    fn reset_initial_layout(&self) -> ApiResult<Layout>;

    // 界面框架（core）
    fn overview(&self) -> ApiResult<Overview>;
    fn pause_status(&self) -> ApiResult<PauseStatus> {
        self.overview().map(|o| o.pause)
    }

    fn set_focus_locked(&self, locked: bool) -> ApiResult<()>;
    /// Admit a handoff to this target followed by a group focus lock.
    fn lock_focus_to(&self, device: DeviceKey) -> ApiResult<()>;
    /// Explicitly recall the pointer, ending any group focus lock first.
    fn return_pointer_home(&self) -> ApiResult<()>;
    fn game_mode(&self) -> ApiResult<GameModeStatus>;
    fn set_screen_disabled(&self, _device: DeviceKey, _disabled: bool) -> ApiResult<()>;
    fn set_input_transform(&self, _device: DeviceKey, _settings: InputTransform) -> ApiResult<()>;
    fn set_keyboard_disabled(&self, _device: DeviceKey, _disabled: bool) -> ApiResult<()>;
    /// Queue a mode request for the current focus device. Success does not mean
    /// the input owner has applied it; observe `game_mode` or `ApiEvent::GameMode`.
    fn set_game_mode(&self, _mode: oink_routing::GameMode) -> ApiResult<()>;
    /// Queue On/Off based on the latest observed active state, with the same
    /// asynchronous completion semantics as `set_game_mode`.
    fn toggle_game_mode(&self) -> ApiResult<()> {
        let on = self.game_mode()?.active;
        self.set_game_mode(if on {
            oink_routing::GameMode::Off
        } else {
            oink_routing::GameMode::On
        })
    }

    // Aiming exercise.
    fn aim_status(&self) -> ApiResult<AimStatus>;
    /// 开始一局：组内每台在线设备的每块屏幕盖上全屏叠加层，本机计分、计时。已有一局在进行时报错。
    fn aim_start(&self) -> ApiResult<AimStatus>;
    /// 结束当前这一局（本机发起的：整局结束；其它设备发起的：请它结束）。没有进行中的游戏时什么也不做。
    fn aim_stop(&self) -> ApiResult<()>;

    // 设备与配对（net）
    fn group(&self) -> ApiResult<GroupView>;
    /// 改名：本机或本组其他设备（改名在组内同步）。
    fn rename_device(&self, device: DeviceKey, name: String) -> ApiResult<()>;

    /// 从本组移除一台设备（被移除的设备无法再连回来）。
    fn remove_member(&self, device: DeviceKey) -> ApiResult<()>;
    /// Explicitly leave the group and become independent.
    fn leave_group(&self) -> ApiResult<()>;

    fn network_info(&self) -> ApiResult<NetworkInfo>;

    // Shared layout geometry.
    fn layout(&self) -> ApiResult<Layout>;
    /// 保存完整布局（精确采用位置与尺寸，可用于撤销/恢复测量结果），同步给组内所有设备。
    fn save_layout(&self, placements: Vec<Placement>) -> ApiResult<Layout>;
    fn set_screen_gain(&self, screen: ScreenKey, gain: f64) -> ApiResult<()>;
    fn set_screen_remote_gain(&self, screen: ScreenKey, gain: f64) -> ApiResult<()>;
    fn set_screen_hidden(&self, screen: ScreenKey, hidden: bool) -> ApiResult<()>;
    /// Briefly identify screens on this device and attached peers.
    fn identify_screens(&self) -> ApiResult<()>;
    /// 各屏幕（本机的与其他设备的）当前桌面壁纸的缩略图。`refresh` 为真时另外让服务重新读一次本机的
    /// 壁纸（界面打开布局页时；不等它读完，有变化时发 [`ApiEvent::Wallpapers`]）。
    fn wallpapers(&self, _refresh: bool) -> ApiResult<Vec<ScreenWallpaper>>;

    fn layout_projection(&self) -> ApiResult<ProjectionStatus>;
    fn set_layout_projection(&self, enabled: bool) -> ApiResult<ProjectionStatus>;
    fn preview_layout_projection(
        &self,
        gesture: u64,
        placements: Vec<Placement>,
        affected: Vec<ScreenKey>,
        ended: bool,
        arrangement: Option<Box<crate::layout_profiles::types::ArrangementSnapshot>>,
    ) -> ApiResult<()>;

    // Layout editing delegates to the corresponding layout geometry operations.
    /// Preview a screen origin in reference-ratio coordinates without changing the layout.
    /// Return None when the screen cannot be placed at that position.
    fn preview_move_screen(&self, screen: ScreenKey, origin: Point) -> ApiResult<Option<LayoutGeometry>>;
    /// 整台设备拖动 `delta` 的预览（`Layout::preview_move_device`）。
    fn preview_move_device(&self, device: DeviceKey, delta: Point) -> ApiResult<Option<LayoutGeometry>>;
    /// 放下（`Layout::move_screen`），保存并同步。
    fn move_screen(&self, screen: ScreenKey, origin: Point) -> ApiResult<Layout>;
    /// 整台设备放下（`Layout::move_device`），保存并同步。
    fn move_device(&self, device: DeviceKey, delta: Point) -> ApiResult<Layout>;
    /// 用户交互缩放（`Layout::resize_screen`，保持当前宽高比，来源变为 `Manual`，然后修复重叠），保存并同步。
    fn set_screen_size(&self, screen: ScreenKey, size: Size) -> ApiResult<Layout>;

    // 输入设置（core）
    /// One local default-off reliability grant; never synchronized to peers.
    fn submit_crash_capsule(
        &self,
        _generation: u64,
        _crash: oink_telemetry::SafeCrash,
        _collected_at: u64,
    ) -> ApiResult<()>;
    fn report_status(&self) -> ApiResult<crate::telemetry::ReportStatus>;
    fn set_report_consent(
        &self,
        _expected_generation: u64,
        _choice: oink_telemetry::Choice,
        _policy_version: u16,
    ) -> ApiResult<()>;
    fn input_settings(&self) -> ApiResult<InputSettings>;
    fn set_input_settings(&self, settings: PreferencePatch<InputSettings>) -> ApiResult<()>;

    // 剪贴板设置（net / core）
    /// Group audio outputs, active route, per-source loss and delay budget.
    fn audio_status(&self) -> ApiResult<oink_audio::Status>;
    /// Explicit list-open refresh; ordinary status sampling stays cache-only.
    fn refresh_audio_devices(&self) -> ApiResult<()>;
    /// Enabling with an exit selected starts capture on every source in the group.
    fn set_audio_settings(&self, _settings: PreferencePatch<oink_audio::Settings>) -> ApiResult<()>;
    fn cache_usage(&self, _device: Option<DeviceKey>) -> ApiResult<Vec<CacheDevice>>;
    fn clear_cache(&self, _device: Option<DeviceKey>) -> ApiResult<CacheDeviceClear>;
    fn received_files_usage(&self) -> ApiResult<ReceivedFilesUsage> {
        Err(ApiError::Unsupported {
            message: "received files".into(),
        })
    }
    fn set_received_files_cleanup(&self, _cleanup: ReceivedFilesCleanup) -> ApiResult<()> {
        Err(ApiError::Unsupported {
            message: "received files".into(),
        })
    }
    fn open_received_files(&self) -> ApiResult<()> {
        Err(ApiError::Unsupported {
            message: "received files".into(),
        })
    }
    fn clipboard_settings(&self) -> ApiResult<ClipboardSettings>;
    fn set_clipboard_settings(&self, settings: PreferencePatch<ClipboardSettings>) -> ApiResult<()>;
    /// 最近的同步记录（新的在前，最多 50 条）。
    fn clipboard_activity(&self) -> ApiResult<Vec<ClipActivity>>;
    fn clipboard_transfer(&self) -> ApiResult<Option<FileTransferStatus>>;
    fn fetch_clipboard_files(&self) -> ApiResult<()>;
    fn cancel_clipboard_files(&self) -> ApiResult<()>;
    /// Cached status; subscribing clients should also query once on connection/reconnection.
    fn file_provider_status(&self) -> ApiResult<FileProviderStatus>;
    /// Queue opening the hidden Oink root in Finder, never click the system Enable button.
    fn open_file_provider_enable(&self) -> ApiResult<()>;
    fn file_provider_placeholder_pool(&self) -> ApiResult<FileProviderPlaceholderStatus>;
    fn set_file_provider_placeholder_pool(&self, _enabled: bool) -> ApiResult<()>;

    // 状态与诊断（core）
    fn update_status(&self) -> ApiResult<UpdateStatus>;
    fn update_command(&self, _command: UpdateCommand) -> ApiResult<UpdateStatus>;
    fn connection_quality(&self) -> ApiResult<ConnectionQuality>;
    fn audio_quality(&self) -> ApiResult<AudioQuality>;
    fn status(&self) -> ApiResult<StatusView>;
    /// 序号大于 `after` 的日志，最多 `limit` 条（取最新的）。
    fn logs(&self, after: Option<u64>, limit: u32) -> ApiResult<Vec<LogLine>>;
    fn export_diagnostics(&self) -> ApiResult<DiagnosticsBundle> {
        self.export_diagnostics_range(Some(30))
    }
    fn export_diagnostics_range(&self, last_minutes: Option<u32>) -> ApiResult<DiagnosticsBundle>;
    fn diagnostics_folder(&self) -> ApiResult<String>;
    fn diagnostics_status(&self) -> ApiResult<DiagnosticRecorderStatus>;
    fn set_diagnostics_enabled(&self, _enabled: bool) -> ApiResult<()>;

    // 权限与引导（core）
    fn permissions(&self) -> ApiResult<Vec<PermissionItem>>;
    /// 触发系统的权限询问（例如 macOS 辅助功能的弹窗、本地网络的首次发现）。
    fn request_permission(&self, kind: PermissionKind) -> ApiResult<()>;
    /// 打开系统设置中对应的面板（[`PermissionKind::macos_settings_url`]）。
    fn open_permission_settings(&self, kind: PermissionKind) -> ApiResult<()>;
    fn complete_onboarding(&self) -> ApiResult<()>;

    // OS-owned login registration.
    /// 系统里登录项的当前状态。只读、每次都问系统（用户可能在系统设置里关掉了它），不保存在 Oink 的设置里。
    fn launch_at_login_status(&self) -> ApiResult<LaunchAtLoginStatus>;
    /// Apply a Settings edit or forward onboarding confirmation, returning OS state.
    /// Confirming also durably records that the default has been accepted or edited.
    fn set_launch_at_login(&self, _enabled: bool) -> ApiResult<LaunchAtLoginStatus>;
    /// 打开系统里管理登录项的设置页。
    fn open_launch_at_login_settings(&self) -> ApiResult<()>;
    /// 让界面把主窗口显示到前台（发出 [`ApiEvent::ShowWindow`]）。Windows 上界面已在托盘里运行时，
    /// 用户再次打开 Oink 的那个进程用它通知已有的界面，然后自己退出。
    fn show_window(&self) -> ApiResult<()>;

    // Process lifecycle and local group recovery.
    /// 「重新打开 Oink」：退出全部进程（后台、界面；Windows 上含托盘加界面进程），然后重新启动后台和界面。
    /// 界面、托盘（菜单栏）、命令行都调它。Mac：后台启动一个带「用户要求重新打开」记号的新的自己，再按正常
    /// 顺序退出。Windows：由托盘加界面进程完成；它不在时返回明确的错误（命令行另有自己的「重新打开」子命令）。
    /// 返回时旧进程还没有退出。
    fn reopen_oink(&self) -> ApiResult<()>;
    /// 「重置回初始状态」：只在组状态读不出来（[`AlertKind::GroupStateUnreadable`]）时可用。旧文件改名保留，
    /// 写入一个新的单机组（新的组编号和组密钥、默认设置），然后启动共享。要重新配对。
    fn reset_group_state(&self) -> ApiResult<()>;
    // 事件
    fn subscribe(&self, sink: EventSink) -> SubscriptionId;
    fn unsubscribe(&self, id: SubscriptionId);
}

// ─────────────────────── 请求 / 应答（线上格式） ───────────────────────

/// 一次 API 调用，与 [`AgentApi`] 的方法一一对应。JSON：`{"method": "connect_device", "params": {"device": "..."}}`。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
pub enum Request {
    LayoutProfiles,
    CreateLayoutProfile(crate::layout_profiles::types::Create),
    RenameLayoutProfile(crate::layout_profiles::types::Rename),
    DeleteLayoutProfile(crate::layout_profiles::types::Delete),
    LoadLayoutProfile(crate::layout_profiles::types::Load),
    LayoutProfileOperation(crate::layout_profiles::types::OperationRequest),
    SetSyncSystemArrangement(crate::layout_profiles::types::SettingRequest),
    PreviewSystemArrangement(crate::layout_profiles::types::PreviewRequest),
    CommitArrangement(crate::layout_profiles::types::CommitRequest),
    ResetArrangement(crate::layout_profiles::types::Mutation),
    ReplayArrangement(crate::layout_profiles::types::ReplayRequest),

    ConnectionQuality,
    AudioQuality,
    DeviceCards,
    #[serde(rename_all = "camelCase")]
    InvokeDeviceCardAction {
        request_id: String,
        token: String,
    },
    #[serde(rename_all = "camelCase")]
    SetForwardingControls {
        request_id: String,
        version_token: String,
        volume_percent: u16,
        muted: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        defer_persistence: bool,
    },
    #[serde(rename_all = "camelCase")]
    SetExitVolume {
        request_id: String,
        version_token: String,
        volume_percent: u8,
        muted: bool,
    },
    #[serde(rename_all = "camelCase")]
    SetLocalPaused {
        request_id: String,
        paused: bool,
    },
    #[serde(rename_all = "camelCase")]
    SetDevicePaused {
        request_id: String,
        token: String,
        paused: bool,
    },
    #[serde(rename_all = "camelCase")]
    PauseAll {
        request_id: String,
    },
    MemberAddresses {
        device: DeviceKey,
    },
    SetMemberAddress {
        device: DeviceKey,
        address: String,
    },
    Nearby,
    RefreshNearby {
        operation_id: String,
    },
    QueryPairingAddress {
        operation_id: String,
        generation: crate::device_cards::DecimalU64,
        ip: String,
        port: Option<u16>,
    },
    ReleasePairingQuery {
        query_id: String,
        generation: crate::device_cards::DecimalU64,
    },
    StartPairing {
        operation_id: String,
        target: pairing::PairTarget,
    },
    Pairings,
    SetOnboardingConnect {
        active: bool,
    },
    GroupMigration,
    AcceptPairing {
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    },
    DeclinePairing {
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    },
    CancelPairing {
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    },
    PairingCode {
        pair_id: String,
    },
    SubmitPairingCode {
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
        attempt: u8,
        code: String,
    },
    UndoPairing {
        operation_id: String,
        handle: pairing::UndoHandle,
    },
    SubscribePairingEvents {
        after_event_id: crate::device_cards::DecimalU64,
    },
    AckPairingEvent {
        event_id: crate::device_cards::DecimalU64,
    },
    BlockedDevices,
    Block {
        operation_id: String,
        device: DeviceKey,
    },
    Unblock {
        operation_id: String,
        device: DeviceKey,
    },
    Notify {
        notification: Notification,
    },
    UiPreferences,
    SetUiPreferences {
        settings: PreferencePatch<UiPreferences>,
    },
    QueryUpdateStatus,
    Update {
        command: UpdateCommand,
    },
    ReportStatus,
    SubmitCrashCapsule {
        generation: u64,
        crash: oink_telemetry::SafeCrash,
        collected_at: u64,
    },
    SetReportConsent {
        expected_generation: u64,
        choice: oink_telemetry::Choice,
        policy_version: u16,
    },
    AudioStatus,
    RefreshAudioDevices,
    SetAudioSettings {
        settings: PreferencePatch<oink_audio::Settings>,
    },
    Overview,
    PauseStatus,
    LockFocusTo {
        device: DeviceKey,
    },
    SetFocusLocked {
        locked: bool,
    },
    ReturnPointerHome,
    GameMode,
    SetScreenDisabled {
        device: DeviceKey,
        disabled: bool,
    },
    SetInputTransform {
        device: DeviceKey,
        settings: InputTransform,
    },
    SetKeyboardDisabled {
        device: DeviceKey,
        disabled: bool,
    },
    SetGameMode {
        mode: oink_routing::GameMode,
    },
    ToggleGameMode,
    Group,
    RenameDevice {
        device: DeviceKey,
        name: String,
    },
    RemoveMember {
        device: DeviceKey,
    },
    LeaveGroup,
    NetworkInfo,
    Layout,
    ResetInitialLayout,
    SaveLayout {
        placements: Vec<Placement>,
    },
    SetScreenRemoteGain {
        screen: ScreenKey,
        gain: f64,
    },
    SetScreenGain {
        screen: ScreenKey,
        gain: f64,
    },
    SetScreenHidden {
        screen: ScreenKey,
        hidden: bool,
    },
    IdentifyScreens,
    LayoutProjection,
    SetLayoutProjection {
        enabled: bool,
    },
    PreviewLayoutProjection {
        gesture: u64,
        placements: Vec<Placement>,
        affected: Vec<ScreenKey>,
        ended: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arrangement: Option<Box<crate::layout_profiles::types::ArrangementSnapshot>>,
    },
    PreviewMoveScreen {
        screen: ScreenKey,
        origin: Point,
    },
    PreviewMoveDevice {
        device: DeviceKey,
        delta: Point,
    },
    MoveScreen {
        screen: ScreenKey,
        origin: Point,
    },
    MoveDevice {
        device: DeviceKey,
        delta: Point,
    },
    SetScreenSize {
        screen: ScreenKey,
        size: Size,
    },
    InputSettings,
    SetInputSettings {
        settings: PreferencePatch<InputSettings>,
    },
    CacheUsage {
        device: Option<DeviceKey>,
    },
    ClearCache {
        device: Option<DeviceKey>,
    },
    ReceivedFilesUsage,
    SetReceivedFilesCleanup {
        cleanup: ReceivedFilesCleanup,
    },
    OpenReceivedFiles,
    ClipboardSettings,
    SetClipboardSettings {
        settings: PreferencePatch<ClipboardSettings>,
    },
    ClipboardActivity,
    ClipboardTransfer,
    FetchClipboardFiles,
    CancelClipboardFiles,
    FileProviderStatus,
    OpenFileProviderEnable,
    Status,
    Logs {
        after: Option<u64>,
        limit: u32,
    },
    ExportDiagnostics {
        #[serde(default = "default_diagnostics_minutes")]
        last_minutes: Option<u32>,
    },
    DiagnosticsFolder,
    DiagnosticsStatus,
    SetDiagnosticsEnabled {
        enabled: bool,
    },
    Permissions,
    RequestPermission {
        kind: PermissionKind,
    },
    OpenPermissionSettings {
        kind: PermissionKind,
    },
    CompleteOnboarding,
    LaunchAtLoginStatus,
    SetLaunchAtLogin {
        enabled: bool,
    },
    OpenLaunchAtLoginSettings,
    ShowWindow,
    AimStatus,
    AimStart,
    AimStop,
    Wallpapers {
        #[serde(default)]
        refresh: bool,
    },
    // 链路重做（追加在末尾）
    ReopenOink,
    ResetGroupState,
    FileProviderPlaceholderPool,
    SetFileProviderPlaceholderPool {
        enabled: bool,
    },
}

/// 调用结果。序列化时只输出内容本身（无标签），`Unit` 为 `null`。
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum Reply {
    LayoutProfile(serde_json::Value),
    ConnectionQuality(ConnectionQuality),
    AudioQuality(AudioQuality),
    DeviceCards(crate::device_cards::DeviceCardsView),
    InvokeDeviceCardAction(crate::device_cards::DeviceControlReply),
    PauseAll(crate::device_cards::PauseAllAccepted),
    MemberAddresses(Vec<String>),
    Nearby(pairing::NearbySnapshot),
    RefreshNearby(pairing::PairReply<pairing::Accepted>),
    StartPairing(pairing::PairReply<pairing::Started>),
    Pairings(pairing::PairingSnapshot),
    GroupMigration(Option<pairing::GroupMigrationStatus>),
    CancelPairing(pairing::PairReply<pairing::CancelResult>),
    PairingCode(pairing::CodeView),
    SubscribePairingEvents(Vec<pairing::PairingEvent>),
    BlockedDevices(crate::device_cards::BlockedDevicesSnapshot),
    Block(crate::device_cards::BlockReply),
    Notification(Option<NotificationReceipt>),
    UiPreferences(UiPreferences),
    ReportStatus(crate::telemetry::ReportStatus),
    UpdateStatus(UpdateStatus),
    LayoutProjection(ProjectionStatus),
    Audio(oink_audio::Status),
    GameMode(GameModeStatus),
    Aim(AimStatus),
    Unit,
    Overview(Overview),
    Pause(PauseStatus),
    Group(GroupView),
    NetworkInfo(NetworkInfo),
    Layout(Layout),
    InputSettings(InputSettings),
    CacheUsage(Vec<CacheDevice>),
    ReceivedFilesUsage(ReceivedFilesUsage),
    CacheClear(CacheDeviceClear),
    ClipboardSettings(ClipboardSettings),
    ClipboardActivity(Vec<ClipActivity>),
    FileTransfer(Option<FileTransferStatus>),
    FileProviderStatus(FileProviderStatus),
    Status(StatusView),
    Logs(Vec<LogLine>),
    Diagnostics(DiagnosticsBundle),
    DiagnosticsFolder(String),
    DiagnosticsStatus(DiagnosticRecorderStatus),
    ReportStatusChanged(crate::telemetry::ReportStatus),
    LaunchAtLogin(LaunchAtLoginStatus),
    Permissions(Vec<PermissionItem>),
    GeometryOpt(Option<LayoutGeometry>),
    LaunchAtLoginStatus(LaunchAtLoginStatus),
    Wallpapers(Vec<ScreenWallpaper>),
    FileProviderPlaceholderPool(FileProviderPlaceholderStatus),
}

fn default_diagnostics_minutes() -> Option<u32> {
    Some(30)
}

/// 执行一次请求。
pub fn dispatch<A: AgentApi + ?Sized>(api: &A, req: Request) -> ApiResult<Reply> {
    use Request as R;
    let unit = |r: ApiResult<()>| r.map(|()| Reply::Unit);
    match req {
        R::LayoutProfiles => api
            .layout_profile_command(crate::layout_profiles::types::Command::Query)
            .map(Reply::LayoutProfile),
        R::CreateLayoutProfile(v) => api
            .layout_profile_command(crate::layout_profiles::types::Command::Create(v))
            .map(Reply::LayoutProfile),
        R::RenameLayoutProfile(v) => api
            .layout_profile_command(crate::layout_profiles::types::Command::Rename(v))
            .map(Reply::LayoutProfile),
        R::DeleteLayoutProfile(v) => api
            .layout_profile_command(crate::layout_profiles::types::Command::Delete(v))
            .map(Reply::LayoutProfile),
        R::LoadLayoutProfile(v) => api
            .layout_profile_command(crate::layout_profiles::types::Command::Load(v))
            .map(Reply::LayoutProfile),
        R::LayoutProfileOperation(v) => api
            .layout_profile_command(crate::layout_profiles::types::Command::Operation(v))
            .map(Reply::LayoutProfile),
        R::SetSyncSystemArrangement(v) => api
            .layout_profile_command(crate::layout_profiles::types::Command::Setting(v))
            .map(Reply::LayoutProfile),
        R::PreviewSystemArrangement(v) => api
            .layout_profile_command(crate::layout_profiles::types::Command::Preview(v))
            .map(Reply::LayoutProfile),
        R::ResetArrangement(v) => api
            .layout_profile_command(crate::layout_profiles::types::Command::Reset(v))
            .map(Reply::LayoutProfile),
        R::CommitArrangement(v) => api
            .layout_profile_command(crate::layout_profiles::types::Command::Commit(v))
            .map(Reply::LayoutProfile),
        R::ReplayArrangement(v) => api
            .layout_profile_command(crate::layout_profiles::types::Command::Replay(v))
            .map(Reply::LayoutProfile),

        R::DeviceCards => api.device_cards().map(Reply::DeviceCards),
        R::InvokeDeviceCardAction { request_id, token } => api
            .invoke_device_card_action(request_id, token)
            .map(Reply::InvokeDeviceCardAction),
        R::SetForwardingControls {
            request_id,
            version_token,
            volume_percent,
            muted,
            defer_persistence,
        } => api
            .set_forwarding_controls_with_persistence(
                request_id,
                version_token,
                volume_percent,
                muted,
                defer_persistence,
            )
            .map(Reply::InvokeDeviceCardAction),
        R::SetExitVolume {
            request_id,
            version_token,
            volume_percent,
            muted,
        } => api
            .set_exit_volume(request_id, version_token, volume_percent, muted)
            .map(Reply::InvokeDeviceCardAction),
        R::SetLocalPaused { request_id, paused } => api
            .set_local_paused(request_id, paused)
            .map(Reply::InvokeDeviceCardAction),
        R::SetDevicePaused {
            request_id,
            token,
            paused,
        } => api
            .set_device_paused(request_id, token, paused)
            .map(Reply::InvokeDeviceCardAction),
        R::PauseAll { request_id } => api.pause_all(request_id).map(Reply::PauseAll),
        R::MemberAddresses { device } => api.member_addresses(device).map(Reply::MemberAddresses),
        R::SetMemberAddress { device, address } => unit(api.set_member_address(device, address)),
        R::Nearby => api.nearby().map(Reply::Nearby),
        R::RefreshNearby { operation_id } => api.refresh_nearby(operation_id).map(Reply::RefreshNearby),
        R::QueryPairingAddress {
            operation_id,
            generation,
            ip,
            port,
        } => api
            .query_pairing_address(operation_id, generation, ip, port)
            .map(Reply::RefreshNearby),
        R::ReleasePairingQuery { query_id, generation } => unit(api.release_pairing_query(query_id, generation)),
        R::StartPairing { operation_id, target } => api.start_pairing(operation_id, target).map(Reply::StartPairing),
        R::Pairings => api.pairings().map(Reply::Pairings),
        R::SetOnboardingConnect { active } => unit(api.set_onboarding_connect(active)),
        R::GroupMigration => api.group_migration().map(Reply::GroupMigration),
        R::AcceptPairing {
            pair_id,
            expected_revision,
        } => api.accept_pairing(pair_id, expected_revision).map(Reply::RefreshNearby),
        R::DeclinePairing {
            pair_id,
            expected_revision,
        } => api
            .decline_pairing(pair_id, expected_revision)
            .map(Reply::RefreshNearby),
        R::CancelPairing {
            pair_id,
            expected_revision,
        } => api.cancel_pairing(pair_id, expected_revision).map(Reply::CancelPairing),
        R::PairingCode { pair_id } => api.pairing_code(pair_id).map(Reply::PairingCode),
        R::SubmitPairingCode {
            pair_id,
            expected_revision,
            attempt,
            code,
        } => api
            .submit_pairing_code(pair_id, expected_revision, attempt, code)
            .map(Reply::RefreshNearby),
        R::UndoPairing { operation_id, handle } => api.undo_pairing(operation_id, handle).map(Reply::RefreshNearby),
        R::SubscribePairingEvents { after_event_id } => api
            .subscribe_pairing_events(after_event_id)
            .map(Reply::SubscribePairingEvents),
        R::AckPairingEvent { event_id } => unit(api.ack_pairing_event(event_id)),
        R::BlockedDevices => api.blocked_devices().map(Reply::BlockedDevices),
        R::Block { operation_id, device } => api.block(operation_id, device).map(Reply::Block),
        R::Unblock { operation_id, device } => api.unblock(operation_id, device).map(Reply::Block),
        R::Notify { notification } => api.notify(notification).map(Reply::Notification),
        R::UiPreferences => api.ui_preferences().map(Reply::UiPreferences),
        R::SetUiPreferences { settings } => api.set_ui_preferences(settings).map(Reply::UiPreferences),
        R::AudioStatus => api.audio_status().map(Reply::Audio),
        R::RefreshAudioDevices => unit(api.refresh_audio_devices()),
        R::SetAudioSettings { settings } => unit(api.set_audio_settings(settings)),
        R::Overview => api.overview().map(Reply::Overview),
        R::PauseStatus => api.pause_status().map(Reply::Pause),
        R::SetFocusLocked { locked } => unit(api.set_focus_locked(locked)),
        R::LockFocusTo { device } => unit(api.lock_focus_to(device)),
        R::ReturnPointerHome => unit(api.return_pointer_home()),
        R::GameMode => api.game_mode().map(Reply::GameMode),
        R::SetScreenDisabled { device, disabled } => unit(api.set_screen_disabled(device, disabled)),
        R::SetInputTransform { device, settings } => unit(api.set_input_transform(device, settings)),
        R::SetKeyboardDisabled { device, disabled } => unit(api.set_keyboard_disabled(device, disabled)),
        R::SetGameMode { mode } => unit(api.set_game_mode(mode)),
        R::ToggleGameMode => unit(api.toggle_game_mode()),
        R::Group => api.group().map(Reply::Group),
        R::RenameDevice { device, name } => unit(api.rename_device(device, name)),
        R::RemoveMember { device } => unit(api.remove_member(device)),
        R::LeaveGroup => unit(api.leave_group()),
        R::NetworkInfo => api.network_info().map(Reply::NetworkInfo),
        R::ResetInitialLayout => api.reset_initial_layout().map(Reply::Layout),
        R::Layout => api.layout().map(Reply::Layout),
        R::SaveLayout { placements } => api.save_layout(placements).map(Reply::Layout),
        R::SetScreenGain { screen, gain } => unit(api.set_screen_gain(screen, gain)),
        R::SetScreenRemoteGain { screen, gain } => unit(api.set_screen_remote_gain(screen, gain)),
        R::SetScreenHidden { screen, hidden } => unit(api.set_screen_hidden(screen, hidden)),
        R::IdentifyScreens => unit(api.identify_screens()),
        R::AimStatus => api.aim_status().map(Reply::Aim),
        R::AimStart => api.aim_start().map(Reply::Aim),
        R::AimStop => unit(api.aim_stop()),
        R::Wallpapers { refresh } => api.wallpapers(refresh).map(Reply::Wallpapers),
        R::LayoutProjection => api.layout_projection().map(Reply::LayoutProjection),
        R::SetLayoutProjection { enabled } => api.set_layout_projection(enabled).map(Reply::LayoutProjection),
        R::PreviewLayoutProjection {
            gesture,
            placements,
            affected,
            ended,
            arrangement,
        } => unit(api.preview_layout_projection(gesture, placements, affected, ended, arrangement)),
        R::PreviewMoveScreen { screen, origin } => api.preview_move_screen(screen, origin).map(Reply::GeometryOpt),
        R::PreviewMoveDevice { device, delta } => api.preview_move_device(device, delta).map(Reply::GeometryOpt),
        R::MoveScreen { screen, origin } => api.move_screen(screen, origin).map(Reply::Layout),
        R::MoveDevice { device, delta } => api.move_device(device, delta).map(Reply::Layout),
        R::SetScreenSize { screen, size } => api.set_screen_size(screen, size).map(Reply::Layout),
        R::SubmitCrashCapsule {
            generation,
            crash,
            collected_at,
        } => unit(api.submit_crash_capsule(generation, crash, collected_at)),
        R::ReportStatus => api.report_status().map(Reply::ReportStatus),
        R::SetReportConsent {
            expected_generation,
            choice,
            policy_version,
        } => unit(api.set_report_consent(expected_generation, choice, policy_version)),
        R::InputSettings => api.input_settings().map(Reply::InputSettings),
        R::SetInputSettings { settings } => unit(api.set_input_settings(settings)),
        R::CacheUsage { device } => api.cache_usage(device).map(Reply::CacheUsage),
        R::ClearCache { device } => api.clear_cache(device).map(Reply::CacheClear),
        R::ReceivedFilesUsage => api.received_files_usage().map(Reply::ReceivedFilesUsage),
        R::SetReceivedFilesCleanup { cleanup } => unit(api.set_received_files_cleanup(cleanup)),
        R::OpenReceivedFiles => unit(api.open_received_files()),
        R::ClipboardSettings => api.clipboard_settings().map(Reply::ClipboardSettings),
        R::SetClipboardSettings { settings } => unit(api.set_clipboard_settings(settings)),
        R::ClipboardActivity => api.clipboard_activity().map(Reply::ClipboardActivity),
        R::ClipboardTransfer => api.clipboard_transfer().map(Reply::FileTransfer),
        R::FetchClipboardFiles => unit(api.fetch_clipboard_files()),
        R::CancelClipboardFiles => unit(api.cancel_clipboard_files()),
        R::FileProviderStatus => api.file_provider_status().map(Reply::FileProviderStatus),
        R::OpenFileProviderEnable => unit(api.open_file_provider_enable()),
        R::FileProviderPlaceholderPool => api
            .file_provider_placeholder_pool()
            .map(Reply::FileProviderPlaceholderPool),
        R::SetFileProviderPlaceholderPool { enabled } => unit(api.set_file_provider_placeholder_pool(enabled)),
        R::QueryUpdateStatus => api.update_status().map(Reply::UpdateStatus),
        R::Update { command } => api.update_command(command).map(Reply::UpdateStatus),
        R::ConnectionQuality => api.connection_quality().map(Reply::ConnectionQuality),
        R::AudioQuality => api.audio_quality().map(Reply::AudioQuality),
        R::Status => api.status().map(Reply::Status),
        R::Logs { after, limit } => api.logs(after, limit).map(Reply::Logs),
        R::ExportDiagnostics { last_minutes } => api.export_diagnostics_range(last_minutes).map(Reply::Diagnostics),
        R::DiagnosticsFolder => api.diagnostics_folder().map(Reply::DiagnosticsFolder),
        R::DiagnosticsStatus => api.diagnostics_status().map(Reply::DiagnosticsStatus),
        R::SetDiagnosticsEnabled { enabled } => unit(api.set_diagnostics_enabled(enabled)),
        R::Permissions => api.permissions().map(Reply::Permissions),
        R::RequestPermission { kind } => unit(api.request_permission(kind)),
        R::OpenPermissionSettings { kind } => unit(api.open_permission_settings(kind)),
        R::CompleteOnboarding => unit(api.complete_onboarding()),
        R::LaunchAtLoginStatus => api.launch_at_login_status().map(Reply::LaunchAtLoginStatus),
        R::SetLaunchAtLogin { enabled } => api.set_launch_at_login(enabled).map(Reply::LaunchAtLoginStatus),
        R::OpenLaunchAtLoginSettings => unit(api.open_launch_at_login_settings()),
        R::ShowWindow => unit(api.show_window()),
        R::ReopenOink => unit(api.reopen_oink()),
        R::ResetGroupState => unit(api.reset_group_state()),
    }
}

/// 解析手动输入的地址：`IP` 或 `IP:端口`（IPv6 带端口时写成 `[addr]:port`）。端口缺省为 `default_port`。
pub fn parse_address(input: &str, default_port: u16) -> ApiResult<std::net::SocketAddr> {
    use std::net::{IpAddr, SocketAddr};
    let s = input.trim();
    if let Ok(sa) = s.parse::<SocketAddr>() {
        return Ok(sa);
    }
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, default_port));
    }
    Err(ApiError::InvalidAddress)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn errors_expose_codes_without_backend_diagnostic_text() {
        let error = ApiError::Internal {
            message: "private native detail".into(),
        };
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["code"], "internal");
        assert_eq!(value["problem"]["code"], "backend_failed");
        assert!(value.get("message").is_none());
        let decoded: ApiError = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.code(), "internal");
        assert!(decoded.message().is_empty());
        assert_eq!(parse_address("bad address", 1), Err(ApiError::InvalidAddress));
    }

    #[test]
    fn address_errors_survive_ipc_without_becoming_backend_unavailable() {
        for (error, code) in [
            (ApiError::InvalidAddress, "invalid_address"),
            (ApiError::NotMember, "not_member"),
            (ApiError::PersistenceFailed, "persistence_failed"),
            (ApiError::NetworkStopped, "network_stopped"),
        ] {
            let json = serde_json::to_value(&error).unwrap();
            assert_eq!(json["problem"]["code"], code);
            assert_eq!(serde_json::from_value::<ApiError>(json).unwrap(), error);
        }
        let device = DeviceKey(DeviceId([9; 32]));
        for request in [
            Request::MemberAddresses { device },
            Request::SetMemberAddress {
                device,
                address: "192.0.2.4:47470".into(),
            },
        ] {
            assert_eq!(
                serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(),
                request
            );
        }
    }

    #[test]
    fn peer_contacts_status_maps_setting_and_exact_advertised_capacity() {
        use super::PeerContactsPath;
        for capacity in [None, Some(0), Some(2), Some(4), Some(5), Some(6)] {
            assert_eq!(
                PeerContactsPath::derive(false, capacity),
                PeerContactsPath::ShortcutsDisabledBySetting
            );
            assert_eq!(
                PeerContactsPath::derive(true, capacity),
                if capacity == Some(5) {
                    PeerContactsPath::Contacts
                } else {
                    PeerContactsPath::ShortcutsTargetUnsupported
                }
            );
        }
        let local = serde_json::to_value(oink_input::ContactInjectionStatus::from_capacity(None)).unwrap();
        assert_eq!(local["init_error_kind"], "platform_unsupported");
        assert_eq!(local["max_contacts"], serde_json::Value::Null);
        assert_eq!(serde_json::to_value(PeerContactsPath::Contacts).unwrap(), "contacts");
        assert_eq!(
            serde_json::to_value(PeerContactsPath::ShortcutsDisabledBySetting).unwrap(),
            "shortcuts_disabled_by_setting"
        );
        assert_eq!(
            serde_json::to_value(PeerContactsPath::ShortcutsTargetUnsupported).unwrap(),
            "shortcuts_target_unsupported"
        );
    }
    #[test]
    fn game_mode_json_reports_hiding_cause_without_disabling_auto() {
        let status = super::GameModeStatus::from(oink_routing::GameModeStatus {
            holder: oink_foundation::DeviceId([1; 32]),
            epoch: 7,
            mode: oink_routing::GameMode::Auto,
            detected: true,
            active: true,
            cursor_hiding_cause_available: Some(false),
        });
        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json["cursor_hiding_cause_available"], false);
        assert_eq!(json["active"], true);
        assert!(json.get("automatic_detection_available").is_none());
    }
    #[test]
    fn placeholder_pool_status_reports_ready_stock_without_inventing_a_drag() {
        let status = FileProviderPlaceholderStatus {
            enabled: true,
            supported: true,
            stock: Some(
                oink_clipboard::provider_preparation::PlaceholderPoolStatus {
                    enabled: true,
                    ready_parents: 2,
                    files_per_parent: 5,
                    folders_per_parent: 5,
                    preparing: false,
                    last_background_failure: None,
                    cleanup_pending: 0,
                }
                .into(),
            ),
        };
        // The CLI prints this untagged IPC reply verbatim. A freshly filled
        // pool is ready stock, even when it has never served a drag.
        assert_eq!(
            serde_json::to_value(Reply::FileProviderPlaceholderPool(status)).unwrap(),
            serde_json::json!({
                "enabled": true,
                "supported": true,
                "stock": {
                    "state": "ready",
                    "ready_parents": 2,
                    "files_per_parent": 5,
                    "folders_per_parent": 5,
                    "preparing": false,
                    "last_background_failure": null,
                    "cleanup_pending": 0
                }
            })
        );
    }
    #[test]
    fn placeholder_pool_stock_state_distinguishes_refill_cleanup_and_failure() {
        use FileProviderPlaceholderState::*;
        let snapshot = |enabled, ready_parents, preparing, cleanup_pending, last_background_failure| {
            oink_clipboard::provider_preparation::PlaceholderPoolStatus {
                enabled,
                ready_parents,
                files_per_parent: 7,
                folders_per_parent: 3,
                preparing,
                last_background_failure,
                cleanup_pending,
            }
        };
        for (enabled, parents, preparing, cleanup, expected) in [
            (false, 0, false, 0, Disabled),
            (true, 0, false, 0, Empty),
            (true, 0, true, 0, Preparing),
            (true, 1, true, 0, Ready),
            (true, 2, false, 0, Ready),
            (false, 0, false, 1, CleanupPending),
            (true, 1, true, 1, CleanupPending),
        ] {
            let stock = FileProviderPlaceholderStock::from(snapshot(enabled, parents, preparing, cleanup, None));
            assert_eq!(stock.state, expected);
            assert_eq!((stock.files_per_parent, stock.folders_per_parent), (7, 3));
            assert_eq!(
                (stock.ready_parents, stock.preparing, stock.cleanup_pending),
                (parents, preparing, cleanup)
            );
        }
        let stock = FileProviderPlaceholderStock::from(snapshot(
            true,
            0,
            false,
            0,
            Some(oink_clipboard::FileProviderReason::RegistrationFailed),
        ));
        assert_eq!(stock.state, Empty);
        assert_eq!(
            stock.last_background_failure,
            Some(oink_clipboard::FileProviderReason::RegistrationFailed)
        );
        assert_eq!(
            serde_json::to_value(Reply::FileProviderPlaceholderPool(
                FileProviderPlaceholderStatus::default()
            ))
            .unwrap(),
            serde_json::json!({"enabled": false, "supported": false, "stock": null})
        );
    }
    #[test]
    fn hex_ids_and_addresses() {
        assert_eq!(parse_hex::<2>("0aFf"), Some([0x0a, 0xff]));
        assert_eq!(parse_hex::<2>("0aF"), None);
        assert_eq!(parse_hex::<1>("zz"), None);
        assert_eq!(
            parse_address("192.0.2.23", 47470).unwrap().to_string(),
            "192.0.2.23:47470"
        );
        assert_eq!(parse_address(" 192.0.2.2:47480 ", 47470).unwrap().port(), 47480);
        assert!(parse_address("[fe80::1]:47470", 1).is_ok());
        assert!(matches!(
            parse_address("192.168.1", 47470),
            Err(ApiError::InvalidAddress)
        ));
    }
}

#[cfg(test)]
mod sharing_state_tests {
    use super::SharingState;
    #[test]
    fn sharing_requires_ready_link_unpaused_and_attached_input() {
        for link_ready in [false, true] {
            for paused in [false, true] {
                for attached in [false, true] {
                    let state = SharingState::derive(link_ready, paused, attached);
                    assert_eq!(state == SharingState::Active, link_ready && !paused && attached);
                    assert_eq!(state == SharingState::Establishing, link_ready && !paused && !attached);
                    assert_eq!(state == SharingState::NotSharing, !link_ready || paused);
                }
            }
        }
    }
}

#[cfg(test)]
mod input_transform_contract_tests {
    use super::*;
    use oink_foundation::{
        input::KeyCode,
        input_transform::{KeyMapping, KeyPreset, KeyboardMapping, TargetOverride},
    };

    #[test]
    fn native_caps_observation_survives_replies_and_events() {
        for observed in [Some(true), Some(false), None] {
            let mut settings = test_input_settings();
            settings.mac_caps_switches_input_source = observed;
            let reply = Reply::InputSettings(settings.clone());
            let decoded: InputSettings = serde_json::from_value(serde_json::to_value(reply).unwrap()).unwrap();
            assert_eq!(decoded.mac_caps_switches_input_source, observed);
            let event = ApiEvent::InputSettings(settings);
            let decoded: ApiEvent = serde_json::from_value(serde_json::to_value(event).unwrap()).unwrap();
            let ApiEvent::InputSettings(decoded) = decoded else {
                panic!("expected input settings event")
            };
            assert_eq!(decoded.mac_caps_switches_input_source, observed);
        }
    }

    #[test]
    fn input_transform_key_support_is_optional_read_only_api_data() {
        let settings = test_input_settings();
        let mut value = serde_json::to_value(&settings).unwrap();
        value.as_object_mut().unwrap().remove("key_support");
        let without: InputSettings = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(without, settings);
        value["key_support"] = serde_json::Value::Bool(false);
        let ignored: InputSettings = serde_json::from_value(value).unwrap();
        assert_eq!(ignored, settings);
    }

    #[test]
    fn cli_self_exclusions_follow_scope_and_are_ignored_in_edits() {
        let source = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let caps = KeyMapping {
            from: KeyCode::CAPS_LOCK,
            to: KeyCode::keyboard(4),
        };
        let function = KeyMapping {
            from: KeyCode::new(0xff, 3),
            to: KeyCode::RIGHT_SHIFT,
        };
        let media = KeyMapping {
            from: KeyCode::keyboard(5),
            to: KeyCode::new(KeyCode::PAGE_CONSUMER, 0xcd),
        };
        let mut native = oink_foundation::input_transform::InputTransform::initial("macos");
        native.macos.keyboard = KeyboardMapping {
            preset: KeyPreset::Custom,
            caps_tap_hold: None,
            table: vec![caps],
        };
        native.overrides = vec![
            TargetOverride {
                target: source,
                reverse_wheel: None,
                keyboard: Some(KeyboardMapping {
                    preset: KeyPreset::Custom,
                    caps_tap_hold: None,
                    table: vec![function],
                }),
            },
            TargetOverride {
                target: peer,
                reverse_wheel: None,
                keyboard: Some(KeyboardMapping {
                    preset: KeyPreset::Custom,
                    caps_tap_hold: None,
                    table: vec![media],
                }),
            },
        ];
        let projected = InputTransform::from(native.clone()).with_local_exclusions(source, "macos");
        assert_eq!(projected.local_exclusions.len(), 2);
        assert_eq!(projected.local_exclusions[0].scope, "macos");
        assert!(
            projected.local_exclusions[0]
                .reason
                .contains("系统设置 > 键盘 > 修饰键")
        );
        assert_eq!(projected.local_exclusions[1].target, Some(source.into()));
        assert!(projected.local_exclusions.iter().all(|i| i.mapping != media));
        let json = serde_json::to_value(projected).unwrap();
        assert_eq!(json["overrides"][0]["target"], source.to_hex());
        let edited: InputTransform = serde_json::from_value(json).unwrap();
        assert!(edited.local_exclusions.is_empty());
        assert_eq!(oink_foundation::input_transform::InputTransform::from(edited), native);
    }
}

#[cfg(test)]
mod product_boundary {
    use super::Request;

    #[test]
    fn request_schema_excludes_development_methods() {
        for value in [
            serde_json::json!({"method": "set_debug_delay", "params": {"peer": null, "config": null}}),
            serde_json::json!({"method": "input_trace_status"}),
            serde_json::json!({"method": "set_input_trace", "params": {"enabled": true}}),
        ] {
            assert!(serde_json::from_value::<Request>(value.clone()).is_err(), "{value}");
        }
        for method in ["overview", "file_provider_placeholder_pool"] {
            assert!(
                serde_json::from_value::<Request>(serde_json::json!({"method": method})).is_ok(),
                "{method}"
            );
        }
        assert!(
            serde_json::from_value::<Request>(serde_json::json!({
                "method": "set_file_provider_placeholder_pool", "params": {"enabled": true}
            }))
            .is_ok()
        );
        assert!(crate::config::Settings::default().file_provider_placeholder_pool);
    }
}

#[cfg(test)]
fn test_input_settings() -> InputSettings {
    InputSettings {
        mac_caps_switches_input_source: None,
        key_support: Default::default(),
        event_frequency: Default::default(),
        accept_contacts: true,
        contacts_injection_available: true,
        mac_scroll_feel: true,
        forward_injected_input: false,
        emergency_hold_ms: 5000,
        device_gain: 1.,
        scroll: ScrollSettings { speed: 1. },
        pointer: PointerSettings {
            reference: PointerReference::LastScreen,
            scaling: PointerScaling::Physical,
            drag_across_devices: false,
        },
        hotkeys: Vec::new(),
        gestures_enabled: true,
        return_pointer_on_display_sleep: true,
        wake_displays_on_input: true,
        cursor_trail: true,
        cursor_trail_duration_ms: 270,
        gap_travel: false,
    }
}
#[cfg(test)]
mod input_validation_tests {
    use super::*;
    #[test]
    fn trail_duration_bounds_are_checked_without_an_api_mock() {
        let mut value = serde_json::to_value(test_input_settings()).unwrap();
        for ms in [100, 270, 60_000] {
            value["cursor_trail_duration_ms"] = serde_json::json!(ms);
            assert_eq!(
                serde_json::from_value::<InputSettings>(value.clone())
                    .unwrap()
                    .cursor_trail_duration_ms,
                ms
            );
        }
        for ms in [0, 99, 60_001] {
            value["cursor_trail_duration_ms"] = serde_json::json!(ms);
            assert!(serde_json::from_value::<InputSettings>(value.clone()).is_err());
        }
    }
}

#[cfg(test)]
mod removed_api_tests {
    use super::*;

    #[test]
    fn removed_methods_and_events_use_generic_unknown_rejection() {
        for method in [
            "calibration",
            "show_calibration_pattern",
            "hide_calibration_pattern",
            "upload_calibration_photo",
            "upload_calibration_photo_from_clipboard",
            "confirm_calibration",
            "cancel_calibration",
            "apply_calibration",
        ] {
            let error = serde_json::from_value::<Request>(serde_json::json!({"method": method})).unwrap_err();
            assert!(error.to_string().contains("unknown variant"), "{method}: {error}");
        }
        assert!(serde_json::from_value::<ApiEvent>(serde_json::json!({"type":"calibration", "data":{}})).is_err());
        assert!(serde_json::from_value::<LayoutSource>(serde_json::json!("calibrated")).is_err());
        assert!(serde_json::from_value::<SizeSource>(serde_json::json!("calibrated")).is_err());
        assert!(serde_json::from_value::<Request>(serde_json::json!({"method":"identify_screens"})).is_ok());
    }
}
