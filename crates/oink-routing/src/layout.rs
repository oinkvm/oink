//! 路由对布局的需求：一个窄接口 [`LayoutQuery`]。
//!
//! 正式实现是 [`crate::display_layout::TopologyLayout`]（适配 `oink_display::layout`，calib 负责）；
//! [`crate::simple_layout::SimpleLayout`] 是给模拟器和单元测试用的简单实现。
//!
//! 坐标约定：
//! - **视平面**：参考单位，y 向下，所有电脑的所有屏幕在同一平面上。
//! - **本机全局坐标**：`DisplayInfo::bounds` 所用的坐标系（macOS point；Windows 物理像素）。

use oink_foundation::{DeviceId, Point};
use serde::{Deserialize, Serialize};

/// 一块屏幕：(设备, `DisplayInfo::key`)。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// 指针位置：某块屏幕 + 该设备的本机全局坐标。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScreenPos {
    pub screen: ScreenId,
    pub local: Point,
}

impl ScreenPos {
    pub fn new(screen: ScreenId, local: Point) -> Self {
        Self { screen, local }
    }
}

/// [`LayoutQuery::step`] 的结果。
#[derive(Clone, Debug, PartialEq)]
pub struct StepResult {
    /// 终点（可能在另一台设备的屏幕上）。
    pub to: ScreenPos,
    /// 途中进入过的屏幕（按顺序，不含起点屏幕）。
    pub crossed: Vec<ScreenId>,
    /// 某个方向被墙挡住（指针沿墙滑动）。
    pub blocked_x: bool,
    pub blocked_y: bool,
}

/// 路由需要的布局查询。实现必须是纯计算（不做 I/O），会在截获回调的快路径里调用。
#[allow(clippy::wrong_self_convention)] // 方法名沿用 calib 的接口草案
pub trait LayoutQuery: Send + Sync {
    /// Whether the effective layout contains a focus target on this device.
    fn contains_device(&self, device: DeviceId) -> bool;
    /// Layout-owned source policy. Geometry may be pending or absent without
    /// changing group membership, native input readiness, OS or audio roles.
    fn source_requires_target(&self, _device: DeviceId) -> bool {
        false
    }
    /// Physical source scale, including sources absent from the target layout.
    fn source_reference_per_native_unit(&self, screen: &ScreenId) -> Option<f64> {
        self.reference_per_native_unit(screen)
    }
    /// Enabled screen centres, nearest to the former holder first. Geometry remains stored.
    fn focus_candidates(&self, _from: DeviceId) -> Vec<ScreenPos> {
        Vec::new()
    }
    /// Ordinary diagnostic projection of the same captured topology; omitted by simple layouts.
    fn first_portal(&self, _from: &ScreenPos, _delta: (f64, f64)) -> Option<oink_display::layout::PortalExit> {
        None
    }
    fn first_portal_from_source(
        &self,
        from: &ScreenPos,
        delta: (f64, f64),
        _source: DeviceId,
    ) -> Option<oink_display::layout::PortalExit> {
        self.first_portal(from, delta)
    }
    /// Extra target-only pointer multiplier, independent of source and scroll scales.
    fn remote_pointer_gain(&self, _screen: &ScreenId, _source: DeviceId) -> f64 {
        1.0
    }
    /// Selected native exit, using the same conversion and corner/wall decision
    /// as exit_through_edge. Simple rectangular layouts have no portal metadata.
    fn first_portal_native(
        &self,
        _from: &ScreenPos,
        _delta_local: (f64, f64),
    ) -> Option<oink_display::layout::PortalExit> {
        None
    }
    /// Production portal geometry for optional gap travel; simple test layouts may omit it.
    fn gap_topology(&self) -> Option<&oink_display::layout::Topology> {
        None
    }

    /// 本机全局坐标 → 视平面 plane。屏幕未知时 `None`。
    fn to_plane(&self, pos: &ScreenPos) -> Option<Point>;
    /// Plane plane → native global coordinates, clipped inside this screen's
    /// half-open native bounds. Use the explicit extrapolation for geometry.
    fn from_plane(&self, screen: &ScreenId, plane: Point) -> Option<Point>;
    /// Plane plane → native global coordinates without clipping, including points
    /// outside this screen. Unknown/unsupported geometry returns None.
    fn from_plane_unclamped(&self, _screen: &ScreenId, _mm: Point) -> Option<Point> {
        None
    }
    /// 指针从 `from` 出发移动 `delta`（「手部 plane」：源机位移 × 源屏 [`LayoutQuery::reference_per_native_unit`]）：
    /// 穿过传送门、被墙挡住（沿墙滑动），返回终点与途中进入过的屏幕。`from` 所在屏幕未知时原地不动。
    fn step(&self, from: &ScreenPos, delta: (f64, f64)) -> StepResult;
    fn step_from_source(&self, from: &ScreenPos, delta: (f64, f64), _source: DeviceId) -> StepResult {
        self.step(from, delta)
    }
    /// 本机光标从 `from` 继续移动 `delta_local`（本机单位，系统已加速）时，是否会穿过传送门进入
    /// **另一块屏幕**；会则返回落点，停在本屏（含撞墙）时为 `None`。落点可能是其他设备的屏幕，
    /// 也可能是本机的另一块屏幕（调用方比较设备 ID）。
    fn exit_through_edge(&self, from: &ScreenPos, delta_local: (f64, f64)) -> Option<ScreenPos>;
    /// 该屏幕每个本机单位对应的「手部 plane」（含按屏增益）。
    fn reference_per_native_unit(&self, screen: &ScreenId) -> Option<f64>;
}
