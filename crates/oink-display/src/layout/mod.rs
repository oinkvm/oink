//! Shared screen geometry in a fixed reference plane. One unit is the frozen
//! reference diagonal; native pointer coordinates remain OS points or pixels.
//!
//! Native refresh and API resizes resolve overlap; auto-compaction belongs to the
//! canvas editor's manual edits. Effective geometry applies owner preferences
//! before topology compiles portals and walls.

use oink_foundation::display::DisplayInfo;
use oink_foundation::{DeviceId, Point, Rect};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod arrange;
mod edit;
pub mod facing;
mod portal;
mod topology;
mod trigger;
mod view;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod trigger_tests;

pub use arrange::{estimate_reference_extent, plausible_reference_extent};
pub use portal::{EdgeInfo, Segment, SegmentKind};
pub use topology::{PortalExit, StepResult, Topology};
pub use trigger::TriggerZone;
pub use view::{LayoutView, ScreenView};

/// 一块屏幕：`(设备 ID, DisplayInfo.key)`。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ScreenId {
    pub device: DeviceId,
    pub display: String,
}

impl ScreenId {
    pub fn new(device: DeviceId, display: impl Into<String>) -> Self {
        Self {
            device,
            display: display.into(),
        }
    }
}

/// 某块屏幕上的一个位置。`local` 为该设备的本机全局坐标。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScreenPos {
    pub screen: ScreenId,
    pub local: Point,
}

/// 屏幕的一条边。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Side {
    Left,
    Top,
    Right,
    Bottom,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Left, Side::Top, Side::Right, Side::Bottom];

    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    /// 左右边的「沿边方向」是 y，上下边是 x。
    pub fn is_vertical(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }

    pub(crate) fn index(self) -> usize {
        match self {
            Side::Left => 0,
            Side::Top => 1,
            Side::Right => 2,
            Side::Bottom => 3,
        }
    }
}

/// 屏幕物理尺寸的来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeSource {
    /// 系统报告（EDID 等）且通过合理性检查。
    Reported = 0,
    /// Estimated from resolution when the system reports no plausible physical size.
    Estimated = 1,
    /// 用户填写或在编辑器里调整。
    Manual = 3,
}

/// Expanded screen geometry. The shared register stores one native-origin
/// transform per computer; `rect` is derived from native `local` bounds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScreenPlacement {
    pub id: ScreenId,
    /// 给人看的名字（来自 `DisplayInfo::name`）。
    pub name: String,
    /// 屏幕在本机全局坐标中的范围（`DisplayInfo::bounds`）。
    pub local: Rect,
    /// 屏幕在视平面上的范围（plane）。
    pub rect: Rect,
    pub size_source: SizeSource,
    /// 是否是所属设备的主屏（`DisplayInfo::is_primary`，给界面标注）。
    #[serde(default)]
    pub primary: bool,
    /// 是否是内建屏（`DisplayInfo::is_builtin`，给界面标注）。
    #[serde(default)]
    pub builtin: bool,
}

impl ScreenPlacement {
    /// 由显示器信息生成，放在视平面 `origin`（左上角）。
    pub fn from_display(device: DeviceId, info: &DisplayInfo, origin: Point) -> Self {
        let (size, size_source) = estimate_reference_extent(info);
        Self {
            id: ScreenId::new(device, info.key.clone()),
            name: info.name.clone(),
            local: info.bounds,
            rect: Rect::new(origin.x, origin.y, size.w, size.h),
            size_source,
            primary: info.is_primary,
            builtin: info.is_builtin,
        }
    }

    /// 视平面 plane / 本机坐标单位，分别按 x、y（不含增益）。
    pub fn physical_scale(&self) -> (f64, f64) {
        (safe_div(self.rect.w, self.local.w), safe_div(self.rect.h, self.local.h))
    }

    /// 本机坐标 → 视平面 plane（不做裁剪）。
    pub fn local_to_plane(&self, p: Point) -> Point {
        let (sx, sy) = self.physical_scale();
        Point::new(
            self.rect.x + (p.x - self.local.x) * sx,
            self.rect.y + (p.y - self.local.y) * sy,
        )
    }

    /// 视平面 plane → 本机坐标（不做裁剪）。
    pub fn plane_to_local(&self, p: Point) -> Point {
        let (sx, sy) = self.physical_scale();
        Point::new(
            self.local.x + safe_div(p.x - self.rect.x, sx),
            self.local.y + safe_div(p.y - self.rect.y, sy),
        )
    }

    /// Translate the screen rectangle in reference units.
    pub fn translate(&mut self, dx: f64, dy: f64) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.rect.x += dx;
        self.rect.y += dy;
    }

    pub(crate) fn is_valid(&self) -> bool {
        let finite = |r: &Rect| r.x.is_finite() && r.y.is_finite() && r.w.is_finite() && r.h.is_finite();
        finite(&self.rect)
            && finite(&self.local)
            && self.rect.w > 0.0
            && self.rect.h > 0.0
            && self.local.w > 0.0
            && self.local.h > 0.0
    }
}

fn safe_div(a: f64, b: f64) -> f64 {
    if b != 0.0 { a / b } else { 0.0 }
}

/// 指针速度模式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpeedMode {
    /// 按视平面物理距离：手部同样的动作，在任何屏幕上看到的指针移动距离一致（默认）。
    #[default]
    Physical,
    /// 按屏幕比例：同样的动作走过屏幕对角线的同样比例（大屏上移动得更远）。
    ScreenProportional,
}

/// Local source-side normal pointer-motion budget; game-relative input is unchanged.
/// The owner preference defaults to 125 Hz.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventFrequency {
    /// Two-event burst capacity, refilling one event every 8 ms. Ordering barriers flush early.
    #[default]
    Hz125,
    /// Send every source motion immediately, without coalescing.
    Unlimited,
}

/// Local ribbon-point lifetime, in milliseconds.
pub const DEFAULT_CURSOR_TRAIL_DURATION_MS: u32 = 270;
pub const MIN_CURSOR_TRAIL_DURATION_MS: u32 = 100;
pub const MAX_CURSOR_TRAIL_DURATION_MS: u32 = 60_000;

pub fn default_cursor_trail_duration_ms() -> u32 {
    DEFAULT_CURSOR_TRAIL_DURATION_MS
}

pub fn valid_cursor_trail_duration_ms(ms: u32) -> bool {
    (MIN_CURSOR_TRAIL_DURATION_MS..=MAX_CURSOR_TRAIL_DURATION_MS).contains(&ms)
}

pub fn deserialize_cursor_trail_duration_ms<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let ms = u32::deserialize(d)?;
    if valid_cursor_trail_duration_ms(ms) {
        Ok(ms)
    } else {
        Err(serde::de::Error::custom("cursor trail duration must be 100–60000 ms"))
    }
}

/// 传送门与墙的规则参数。单位 plane。
///
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutSettings {
    /// Travel through positive gaps between devices, using the leaving screen scale.
    pub gap_travel: bool,
    /// Extra near-miss reach along channel ends, in plane plane (0 disables; maximum 50).
    #[serde(deserialize_with = "deserialize_endpoint_reach")]
    pub endpoint_reach: f64,
    /// Retained topology parameter. Nearest-facing occlusion no longer uses a
    /// blocker threshold; facing tolerates only shared floating-point roundoff.
    pub overlap_tolerance: f64,
    /// 形成传送门所需的最小沿边重叠长度；不用于移动布局中的屏幕。
    pub minimum_portal: f64,
    /// 吸附容差：传送门端点离边端点、或两个传送门之间的墙短于此值时，并入传送门（消除测量噪声）。
    pub portal_snap: f64,
    /// `false` 时关闭墙：有传送门的边上，所有墙都分给最近的传送门（落点贴到目标屏的角落）。
    pub walls: bool,
}

impl Default for LayoutSettings {
    fn default() -> Self {
        Self {
            gap_travel: false,
            endpoint_reach: 0.0,
            overlap_tolerance: 5.0 / 686.0,
            minimum_portal: 10.0 / 686.0,
            portal_snap: 8.0 / 686.0,
            walls: true,
        }
    }
}

pub fn valid_endpoint_reach(plane: f64) -> bool {
    plane.is_finite() && (0.0..=50.0 / 686.0).contains(&plane)
}
pub fn deserialize_endpoint_reach<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let plane = f64::deserialize(d)?;
    if valid_endpoint_reach(plane) {
        Ok(plane)
    } else {
        Err(serde::de::Error::custom(
            "endpoint reach is outside the reference range",
        ))
    }
}

/// Geometry-only settings available to runtime consumers.
pub type EffectiveSettings = LayoutSettings;

/// A device's applied screen preference. Never serialized inside shared geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenPreference {
    /// Target-only multiplier for pointer input from another device.
    pub remote_gain: f64,
    pub enabled: bool,
    pub gain: f64,
}
impl Default for ScreenPreference {
    fn default() -> Self {
        Self {
            remote_gain: 1.0,
            enabled: true,
            gain: 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OwnerLayoutPreferences {
    pub screen_shared: bool,
    pub device_gain: f64,
    pub speed_mode: SpeedMode,
    pub screens: BTreeMap<String, ScreenPreference>,
}
impl Default for OwnerLayoutPreferences {
    fn default() -> Self {
        Self {
            screen_shared: true,
            device_gain: 1.0,
            speed_mode: SpeedMode::Physical,
            screens: BTreeMap::new(),
        }
    }
}
impl OwnerLayoutPreferences {
    pub fn screen(&self, placement: &ScreenPlacement) -> EffectiveScreen {
        let preference = self.screens.get(&placement.id.display).copied().unwrap_or_default();
        EffectiveScreen {
            placement: placement.clone(),
            enabled: self.screen_shared && preference.enabled,
            gain: self.device_gain * preference.gain,
            remote_gain: preference.remote_gain,
            speed_mode: self.speed_mode,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveScreen {
    pub remote_gain: f64,
    pub placement: ScreenPlacement,
    pub enabled: bool,
    pub gain: f64,
    pub speed_mode: SpeedMode,
}
impl std::ops::Deref for EffectiveScreen {
    type Target = ScreenPlacement;
    fn deref(&self) -> &Self::Target {
        &self.placement
    }
}
impl EffectiveScreen {
    pub(crate) fn is_valid(&self) -> bool {
        self.placement.is_valid()
            && self.gain.is_finite()
            && self.gain > 0.0
            && self.remote_gain.is_finite()
            && self.remote_gain > 0.0
    }
}

/// Runtime geometry, without excluded devices or the setting used to exclude them.
///
/// ```compile_fail
/// let layout = oink_display::layout::Layout::default().effective(&Default::default());
/// let _ = &layout.settings().disabled_devices;
/// ```
/// ```compile_fail
/// let topology = oink_display::layout::Layout::default().topology(&Default::default());
/// let _ = &topology.settings().disabled_devices;
/// ```
/// Native source anchors/scales are retained separately: a device with no target
/// screen still captures input at its physical desktop and keeps its physical scale.
#[derive(Clone, Debug)]
pub struct EffectiveLayout {
    pub(crate) screens: Vec<EffectiveScreen>,
    pub(crate) settings: EffectiveSettings,
    source_origins: std::collections::HashMap<DeviceId, Point>,
    source_scales: std::collections::HashMap<ScreenId, f64>,
}
impl EffectiveLayout {
    pub fn screens(&self) -> &[EffectiveScreen] {
        &self.screens
    }
    pub fn settings(&self) -> &EffectiveSettings {
        &self.settings
    }
    pub fn contains_device(&self, device: DeviceId) -> bool {
        self.screens.iter().any(|s| s.enabled && s.id.device == device)
    }
    /// Hiding a screen never redirects its current input focus.
    pub fn source_requires_target(&self, _device: DeviceId) -> bool {
        false
    }
    pub fn topology(&self) -> Topology {
        Topology::from_effective(self, |_| true)
    }
}

pub(super) fn plane_gain(s: &EffectiveScreen) -> f64 {
    let mode = match s.speed_mode {
        SpeedMode::Physical => 1.0,
        SpeedMode::ScreenProportional => s.rect.w.hypot(s.rect.h) / topology::PROPORTIONAL_REFERENCE_DIAGONAL,
    };
    s.gain * mode
}

/// 所有设备所有屏幕的逻辑布局。可序列化，用于界面绘制、持久化与设备间同步。
///
/// Cross-computer collisions are resolved by rigid translation. Native geometry
/// within each computer, including native gaps or overlaps, is never rearranged.
/// 离线设备的屏幕留在 `screens` 里、位置不变，只在 [`Topology`] 里排除。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub screens: Vec<ScreenPlacement>,
    #[serde(default)]
    pub settings: LayoutSettings,
}

impl Layout {
    /// Apply each device's preferences without changing retained shared geometry.
    pub fn effective(&self, owners: &BTreeMap<DeviceId, OwnerLayoutPreferences>) -> EffectiveLayout {
        let mut source_origins = std::collections::HashMap::new();
        let mut source_scales = std::collections::HashMap::new();
        let mut screens = Vec::new();
        let defaults = OwnerLayoutPreferences::default();
        for placement in &self.screens {
            let owner = owners.get(&placement.id.device).unwrap_or(&defaults);
            let s = owner.screen(placement);
            if !s.is_valid() {
                continue;
            }
            if s.primary || !source_origins.contains_key(&s.id.device) {
                source_origins.insert(s.id.device, s.rect.center());
            }
            let (sx, sy) = s.physical_scale();
            source_scales.insert(s.id.clone(), (sx * sy).sqrt() / plane_gain(&s));
            screens.push(s);
        }
        EffectiveLayout {
            screens,
            settings: self.settings.clone(),
            source_origins,
            source_scales,
        }
    }

    pub fn screen(&self, id: &ScreenId) -> Option<&ScreenPlacement> {
        self.screens.iter().find(|s| &s.id == id)
    }

    pub fn screen_mut(&mut self, id: &ScreenId) -> Option<&mut ScreenPlacement> {
        self.screens.iter_mut().find(|s| &s.id == id)
    }

    /// 编译为查询结构（所有设备都视为在线）。
    pub fn topology(&self, owners: &BTreeMap<DeviceId, OwnerLayoutPreferences>) -> Topology {
        Topology::new(self, owners)
    }

    /// 编译为查询结构，只有 `online` 返回 `true` 的设备的屏幕参与传送门（本机也要算在线）。
    pub fn topology_online(
        &self,
        owners: &BTreeMap<DeviceId, OwnerLayoutPreferences>,
        online: impl Fn(&DeviceId) -> bool,
    ) -> Topology {
        Topology::with_online(self, owners, online)
    }

    /// 所有屏幕（含未启用的）在视平面上的包围盒。没有屏幕时为 `None`。
    pub fn bounding_box(&self) -> Option<Rect> {
        bbox(self.screens.iter().map(|s| s.rect))
    }
}

pub(crate) fn bbox(rects: impl IntoIterator<Item = Rect>) -> Option<Rect> {
    let mut it = rects.into_iter();
    let first = it.next()?;
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.right(), first.bottom());
    for r in it {
        x0 = x0.min(r.x);
        y0 = y0.min(r.y);
        x1 = x1.max(r.right());
        y1 = y1.max(r.bottom());
    }
    Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
}
