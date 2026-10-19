//! 给界面的绘制数据：每块屏幕的逻辑矩形、名字与状态。界面只画「屏幕矩形 + 设备名 / 屏幕名」，
//! 不画网格、传送门、墙（2026-09-28 决定）。

use super::{Layout, OwnerLayoutPreferences, ScreenId, bbox};
use oink_foundation::{DeviceId, Rect};
use serde::{Deserialize, Serialize};

/// 一块屏幕的绘制数据。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScreenView {
    pub id: ScreenId,
    /// 所属设备（同 `id.device`；设备名由界面按设备 ID 查）。
    pub device: DeviceId,
    /// 屏幕名（`DisplayInfo::name`）。
    pub name: String,
    /// 逻辑布局中的矩形（plane，x 向右、y 向下）。按物理尺寸，屏幕之间的间隙保留。
    pub rect: Rect,
    /// 所属设备的主屏。
    pub primary: bool,
    /// 内建屏（笔记本屏幕）。
    pub builtin: bool,
    /// 用户是否启用（虚拟显示器默认不启用）。
    pub enabled: bool,
    /// 所属设备是否在线：`false` 时画成「熄屏」样式（位置不变，但不参与穿越）。
    pub online: bool,
}

/// 整个布局的绘制数据。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LayoutView {
    pub screens: Vec<ScreenView>,
    /// 所有屏幕的包围盒（plane），用于缩放到画布；没有屏幕时为 `None`。
    pub bounds: Option<Rect>,
}

impl Layout {
    /// 绘制数据。`online` 判断设备是否在线（本机要算在线），与构建 [`super::Topology::with_online`]
    /// 用的是同一个集合。
    pub fn view(
        &self,
        owners: &std::collections::BTreeMap<DeviceId, OwnerLayoutPreferences>,
        online: impl Fn(&DeviceId) -> bool,
    ) -> LayoutView {
        let screens: Vec<ScreenView> = self
            .screens
            .iter()
            .filter(|s| s.is_valid())
            .map(|s| ScreenView {
                id: s.id.clone(),
                device: s.id.device,
                name: s.name.clone(),
                rect: s.rect,
                primary: s.primary,
                builtin: s.builtin,
                enabled: owners.get(&s.id.device).cloned().unwrap_or_default().screen(s).enabled,
                online: online(&s.id.device),
            })
            .collect();
        LayoutView {
            bounds: bbox(screens.iter().map(|s| s.rect)),
            screens,
        }
    }
}
