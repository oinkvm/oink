//! [`LayoutQuery`] 的正式实现：适配「屏幕与布局」的 [`oink_display::layout`]（calib 负责）。
//!
//! - [`TopologyLayout`]：把编译好的 [`Topology`] 包成 [`LayoutQuery`]（ID 与结果类型的转换；
//!   `Topology` 对未知屏幕返回 `None` 时原地不动）。
//! - [`LayoutModel`]：路由持有的布局数据——各设备的显示器、用户编辑过的布局、在线设备集合，
//!   任何一项变化后重新编译出 [`TopologyLayout`]（配置模块发布给路由）。
//!
//! Layout rules:
//! - **离线设备的屏幕视为不存在**：编译时只保留在线设备（含本机）的屏幕，A | X | B 横排、X 离线时
//!   指针从 A 直接进入 B；X 重新连上后恢复 A → X → B。布局数据里仍保留 X 的位置。
//! - **桥接不设阈值**、**不做角落穿越**：由 `oink_display::layout` 的规则保证。

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use oink_display::layout::{self as dl, Layout, Topology};
use oink_foundation::display::DisplayInfo;
use oink_foundation::{DeviceId, Point};

use crate::layout::{LayoutQuery, ScreenId, ScreenPos, StepResult};

fn to_dl_id(id: &ScreenId) -> dl::ScreenId {
    dl::ScreenId::new(id.device, id.display.clone())
}

fn from_dl_id(id: dl::ScreenId) -> ScreenId {
    ScreenId::new(id.device, id.display)
}

fn to_dl_pos(p: &ScreenPos) -> dl::ScreenPos {
    dl::ScreenPos {
        screen: to_dl_id(&p.screen),
        local: p.local,
    }
}

fn from_dl_pos(p: dl::ScreenPos) -> ScreenPos {
    ScreenPos::new(from_dl_id(p.screen), p.local)
}

/// [`Topology`] 的 [`LayoutQuery`] 适配。
#[derive(Clone, Debug)]
pub struct TopologyLayout {
    topo: Arc<Topology>,
    online: Option<Arc<BTreeSet<DeviceId>>>,
}

impl TopologyLayout {
    pub fn new(topo: Topology) -> Self {
        Self {
            topo: Arc::new(topo),
            online: None,
        }
    }

    /// 直接从布局数据编译（不做在线过滤）。
    pub fn from_layout(layout: &Layout, owners: &BTreeMap<DeviceId, dl::OwnerLayoutPreferences>) -> Self {
        Self::new(Topology::new(layout, owners))
    }

    pub fn from_effective(layout: &dl::EffectiveLayout, online: impl Fn(&DeviceId) -> bool) -> Self {
        Self::new(Topology::from_effective(layout, online))
    }

    /// Apply input-owner availability to precompiled portals without changing geometry.
    pub fn with_online(&self, online: BTreeSet<DeviceId>) -> Self {
        Self {
            topo: self.topo.clone(),
            online: Some(Arc::new(online)),
        }
    }

    fn online(&self, device: &DeviceId) -> bool {
        self.online.as_ref().is_none_or(|online| online.contains(device))
    }

    pub fn input_online(&self, device: DeviceId) -> bool {
        self.online(&device)
    }
    pub fn topology(&self) -> &Topology {
        &self.topo
    }
}

impl LayoutQuery for TopologyLayout {
    fn contains_device(&self, device: DeviceId) -> bool {
        self.topo.contains_device(device)
    }
    fn source_requires_target(&self, device: DeviceId) -> bool {
        self.topo.source_requires_target(device)
    }
    fn source_reference_per_native_unit(&self, screen: &ScreenId) -> Option<f64> {
        self.topo.source_reference_per_native_unit(&to_dl_id(screen))
    }
    fn focus_candidates(&self, from: DeviceId) -> Vec<ScreenPos> {
        let origin = self.topo.source_origin(from).unwrap_or(Point::new(0., 0.));
        let mut screens: Vec<_> = self
            .topo
            .screens()
            .iter()
            .filter(|s| s.id.device != from && self.topo.is_active(&s.id) && self.online(&s.id.device))
            .collect();
        screens.sort_by(|a, b| {
            let distance = |s: &dl::ScreenPlacement| {
                let p = s.rect.center();
                (p.x - origin.x).hypot(p.y - origin.y)
            };
            distance(a)
                .total_cmp(&distance(b))
                .then(a.id.device.cmp(&b.id.device))
                .then(a.id.display.cmp(&b.id.display))
        });
        screens
            .into_iter()
            .map(|s| ScreenPos::new(from_dl_id(s.id.clone()), s.local.center()))
            .collect()
    }
    fn first_portal(&self, from: &ScreenPos, delta: (f64, f64)) -> Option<oink_display::layout::PortalExit> {
        self.topo
            .first_portal_filtered(&to_dl_pos(from), delta, |d| self.online(d))
    }
    fn first_portal_native(&self, from: &ScreenPos, delta: (f64, f64)) -> Option<oink_display::layout::PortalExit> {
        self.topo
            .first_portal_native_filtered(&to_dl_pos(from), delta, |d| self.online(d))
    }
    fn gap_topology(&self) -> Option<&Topology> {
        Some(&self.topo)
    }

    fn to_plane(&self, pos: &ScreenPos) -> Option<Point> {
        self.topo.to_plane(&to_dl_pos(pos))
    }

    fn from_plane(&self, screen: &ScreenId, plane: Point) -> Option<Point> {
        self.topo.from_plane(&to_dl_id(screen), plane)
    }

    fn from_plane_unclamped(&self, screen: &ScreenId, plane: Point) -> Option<Point> {
        self.topo.from_plane_unclamped(&to_dl_id(screen), plane)
    }

    fn step(&self, from: &ScreenPos, delta: (f64, f64)) -> StepResult {
        match self.topo.step_filtered(&to_dl_pos(from), delta, |d| self.online(d)) {
            Some(r) => StepResult {
                to: from_dl_pos(r.to),
                crossed: r.crossed.into_iter().map(from_dl_id).collect(),
                blocked_x: r.blocked_x,
                blocked_y: r.blocked_y,
            },
            // 布局里还没有这块屏幕（布局与显示器列表不同步的瞬间）：原地不动。
            None => StepResult {
                to: from.clone(),
                crossed: Vec::new(),
                blocked_x: true,
                blocked_y: true,
            },
        }
    }

    fn exit_through_edge(&self, from: &ScreenPos, delta_local: (f64, f64)) -> Option<ScreenPos> {
        self.topo
            .exit_through_edge_filtered(&to_dl_pos(from), delta_local, |d| self.online(d))
            .map(from_dl_pos)
    }

    fn reference_per_native_unit(&self, screen: &ScreenId) -> Option<f64> {
        self.topo.reference_per_native_unit(&to_dl_id(screen))
    }
}

/// 路由用的布局数据：已知设备的显示器、布局（自动排列或用户编辑过的）、在线设备集合。
#[derive(Clone, Debug, Default)]
pub struct LayoutModel {
    layout: Layout,
    /// 各设备最近一次通告的显示器。
    displays: BTreeMap<DeviceId, Vec<DisplayInfo>>,
    online: BTreeSet<DeviceId>,
    /// `true`：尚未采用共享布局或用户编辑。新显示器增量加入，已有屏幕不重排。
    /// 用户编辑过（[`LayoutModel::set_layout`]）之后为 `false`，变化按 [`Layout::sync_device`] 增量处理。
    auto: bool,
    /// 本机设备（[`LayoutModel::set_local`]）。非自动模式下只有本机的显示器会改动布局数据：每台设备
    /// 只负责把**自己的**屏幕合进共享布局并发布新版本，别的设备的显示器只记下来，等它的主人发布。
    /// 这样同一个布局版本在组内每台设备上都完全相同（接收方只采用、不再变换）。`None`：不区分（测试）。
    local: Option<DeviceId>,
}

impl LayoutModel {
    /// 初始排列：新设备依次放到已有屏幕右边，不重排已经存在的屏幕。
    pub fn auto() -> Self {
        Self {
            auto: true,
            ..Self::default()
        }
    }

    /// 使用给定的布局（用户编辑或加载的）。之后新出现的设备按 [`Layout::sync_device`] 放到右边。
    pub fn from_layout(layout: Layout) -> Self {
        Self {
            layout,
            auto: false,
            ..Self::default()
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn is_auto(&self) -> bool {
        self.auto
    }

    pub fn online(&self) -> &BTreeSet<DeviceId> {
        &self.online
    }

    /// 设定本机设备（见字段说明）。
    pub fn set_local(&mut self, device: DeviceId) {
        self.local = Some(device);
    }

    /// 非自动模式下 `device` 的显示器是否由本机合进布局。
    fn owns(&self, device: DeviceId) -> bool {
        self.local.is_none_or(|me| me == device)
    }

    /// 替换为用户编辑后的（或从其他设备同步来的）布局，之后不再自动重排。
    ///
    /// 布局原样采用；随后只把**本机**的显示器按 [`Layout::sync_device`] 合进去（例如收到的布局里还没有
    /// 本机的屏幕，或本机的分辨率已经变了）。返回合并后的布局与 `layout` 是否不同——不同时调用方
    /// 要以新版本发布。
    pub fn set_layout(&mut self, layout: Layout) -> bool {
        self.layout = layout.clone();
        self.auto = false;
        for (d, displays) in &self.displays {
            if self.local.is_none_or(|me| me == *d) {
                self.layout.sync_device(*d, displays);
            }
        }
        self.layout != layout
    }

    /// 修改布局数据（界面编辑、标定结果），之后不再自动重排。
    pub fn edit(&mut self, f: impl FnOnce(&mut Layout)) {
        f(&mut self.layout);
        self.auto = false;
    }

    /// 某台设备最近通告的显示器。
    pub fn device_displays(&self, device: &DeviceId) -> Option<&[DisplayInfo]> {
        self.displays.get(device).map(Vec::as_slice)
    }

    /// 某台设备的显示器列表（本机启动 / 变化时，或收到对端的显示事实）。
    /// 返回布局是否变化。
    pub fn set_device_displays(&mut self, device: DeviceId, displays: Vec<DisplayInfo>) -> bool {
        if self.displays.get(&device) == Some(&displays) {
            return false;
        }
        if self.auto || self.owns(device) {
            self.layout.sync_device(device, &displays);
            self.displays.insert(device, displays);
        } else {
            // 别的设备的屏幕由它自己合进布局并发布（见 `local`）：这里只记下来。
            self.displays.insert(device, displays);
            return false;
        }
        true
    }

    /// 设备离开设备组：删除它的所有屏幕。
    pub fn remove_device(&mut self, device: DeviceId) {
        self.displays.remove(&device);
        self.layout.remove_device(device);
    }

    /// 更新在线设备集合（本机也要包含在内）。返回是否变化。
    pub fn set_online(&mut self, online: BTreeSet<DeviceId>) -> bool {
        if self.online == online {
            return false;
        }
        self.online = online;
        true
    }

    /// 编译：离线设备的屏幕视为不存在（`Layout::topology_online`）。
    pub fn compile(&self, owners: &BTreeMap<DeviceId, dl::OwnerLayoutPreferences>) -> TopologyLayout {
        TopologyLayout::new(self.layout.topology_online(owners, |d| self.online.contains(d)))
    }

    /// 给界面的绘制数据（离线设备的屏幕画成熄屏）。
    pub fn view(&self, owners: &BTreeMap<DeviceId, dl::OwnerLayoutPreferences>) -> dl::LayoutView {
        self.layout.view(owners, |d| self.online.contains(d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::{Rect, Size};

    fn dev(n: u8) -> DeviceId {
        DeviceId([n; 32])
    }

    fn display(key: &str, w: f64, h: f64, plane: (f64, f64)) -> DisplayInfo {
        DisplayInfo {
            key: key.into(),
            name: key.into(),
            bounds: Rect::new(0.0, 0.0, w, h),
            pixel_width: w as u32,
            pixel_height: h as u32,
            reference_extent: Some(Size::new(plane.0 / 686.0, plane.1 / 686.0)),
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        }
    }

    fn model3() -> LayoutModel {
        let mut m = LayoutModel::auto();
        m.set_device_displays(dev(1), vec![display("a", 1000.0, 1000.0, (300.0, 300.0))]);
        m.set_device_displays(dev(2), vec![display("x", 1000.0, 1000.0, (300.0, 300.0))]);
        m.set_device_displays(dev(3), vec![display("c", 1000.0, 1000.0, (300.0, 300.0))]);
        m
    }

    #[test]
    fn initial_layout_updates_do_not_repack_existing_screens() {
        let mut m = LayoutModel::auto();
        m.set_device_displays(dev(3), vec![display("c", 1000.0, 1000.0, (300.0, 300.0))]);
        let original = m.layout().screens[0].rect;
        m.set_device_displays(dev(1), vec![display("a", 1000.0, 1000.0, (300.0, 300.0))]);
        assert_eq!(m.layout().screens[0].rect, original);
        assert_eq!(m.layout().screens[1].rect.x, original.right());
        m.set_device_displays(dev(3), vec![]);
        assert_eq!(m.layout().screens[0].rect.x, original.right());
    }

    #[test]
    fn offline_device_screens_are_skipped() {
        let mut m = model3();
        m.set_online([dev(1), dev(2), dev(3)].into());
        let a = ScreenPos::new(ScreenId::new(dev(1), "a"), Point::new(999.0, 500.0));
        let q = m.compile(&Default::default());
        assert_eq!(q.exit_through_edge(&a, (5.0, 0.0)).unwrap().screen.device, dev(2));
        // X 离线：A 的右边直接通往 B（跨过 300/686 reference units 的空位）。
        m.set_online([dev(1), dev(3)].into());
        let q = m.compile(&Default::default());
        let hit = q.exit_through_edge(&a, (5.0, 0.0)).unwrap();
        assert_eq!(hit.screen.device, dev(3));
        assert!(hit.local.x < 20.0 && (hit.local.y - 500.0).abs() < 1e-6, "{hit:?}");
        let r = q.step(&a, (10.0 / 686.0, 0.0 / 686.0));
        assert_eq!(r.to.screen.device, dev(3));
        assert_eq!(r.crossed, vec![ScreenId::new(dev(3), "c")]);
    }

    #[test]
    fn three_screens_with_gaps_cross_in_order_at_the_same_coordinate() {
        let mut m = model3();
        let mut l = m.layout().clone();
        l.screens[1].rect.x = 530.0 / 686.0; // 230/686 reference units gap
        l.screens[1].rect.y = 40.0 / 686.0;
        l.screens[2].rect.x = 1130.0 / 686.0; // 300/686 reference units gap
        l.screens[2].rect.y = -30.0 / 686.0;
        m.set_layout(l);
        m.set_online([dev(1), dev(2), dev(3)].into());
        let q = m.compile(&Default::default());
        let a = ScreenPos::new(ScreenId::new(dev(1), "a"), Point::new(990.0, 500.0));
        let hit = q.exit_through_edge(&a, (20.0, 0.0)).unwrap();
        assert_eq!(hit.screen, ScreenId::new(dev(2), "x"));
        assert!((q.to_plane(&hit).unwrap().y - 150.0 / 686.0).abs() < 1e-6);
        let r = q.step(&a, (320.0 / 686.0, 0.0 / 686.0));
        assert_eq!(r.crossed, vec![ScreenId::new(dev(2), "x"), ScreenId::new(dev(3), "c")]);
        let p = q.to_plane(&r.to).unwrap();
        assert!(
            (p.x - 1147.0 / 686.0).abs() < 1e-6 && (p.y - 150.0 / 686.0).abs() < 1e-6,
            "{p:?}"
        );
        let back = q.step(&r.to, (-320.0 / 686.0, 0.0 / 686.0));
        assert_eq!(
            back.crossed,
            vec![ScreenId::new(dev(2), "x"), ScreenId::new(dev(1), "a")]
        );
        // Crossings inset by a microscopic epsilon to avoid landing on an
        // ambiguous edge; allow that accumulated numerical inset on return.
        assert!((back.to.local.x - a.local.x).abs() < 1e-3, "{:?}", back.to);
        assert!((back.to.local.y - a.local.y).abs() < 1e-3);

        // No screen lies above A, and none lies to the right of C.
        assert!(q.exit_through_edge(&a, (0.0, -1000.0)).is_none());
        let wall = q.step(&a, (0.0 / 686.0, -1000.0 / 686.0));
        assert_eq!(wall.to.screen, a.screen);
        assert!(wall.blocked_y && wall.crossed.is_empty());
        let c = ScreenPos::new(ScreenId::new(dev(3), "c"), Point::new(999.0, 600.0));
        assert!(q.exit_through_edge(&c, (20.0, 0.0)).is_none());
        assert!(q.step(&c, (20.0 / 686.0, 0.0 / 686.0)).blocked_x);
    }

    #[test]
    fn input_mask_turns_precompiled_portals_into_walls_without_bridging() {
        let mut m = model3();
        m.set_online([dev(1), dev(2), dev(3)].into());
        let compiled = m.compile(&Default::default());
        let masked = compiled.with_online([dev(1), dev(3)].into());
        assert!(Arc::ptr_eq(&compiled.topo, &masked.topo));
        let a = ScreenPos::new(ScreenId::new(dev(1), "a"), Point::new(999., 500.));
        assert!(masked.exit_through_edge(&a, (100., 0.)).is_none());
        let stopped = masked.step(&a, (1000., 0.));
        assert_eq!(stopped.to.screen, a.screen);
        assert!(stopped.crossed.is_empty());
        assert!(stopped.blocked_x);
        assert!(
            masked
                .topology()
                .first_portal_filtered(&to_dl_pos(&a), (1000., 0.), |d| masked.online(d))
                .is_none()
        );
        let restored = masked.with_online([dev(1), dev(2), dev(3)].into());
        assert!(Arc::ptr_eq(&compiled.topo, &restored.topo));
        assert_eq!(restored.exit_through_edge(&a, (5., 0.)).unwrap().screen.device, dev(2));
    }

    #[test]
    fn unknown_screen_stays_put() {
        let m = model3();
        let q = m.compile(&Default::default()); // 没有在线设备：空拓扑
        let p = ScreenPos::new(ScreenId::new(dev(9), "?"), Point::new(1.0, 2.0));
        let r = q.step(&p, (10.0 / 686.0, 0.0 / 686.0));
        assert_eq!(r.to, p);
        assert!(q.exit_through_edge(&p, (10.0, 0.0)).is_none());
        assert!(q.reference_per_native_unit(&p.screen).is_none());
    }

    #[test]
    fn user_layout_keeps_positions_on_display_change() {
        let mut m = model3();
        let mut l = m.layout().clone();
        l.move_screen(&dl::ScreenId::new(dev(3), "c"), Point::new(0.0, 400.0));
        m.set_layout(l);
        assert!(!m.is_auto());
        // 无重叠的手动位置与间隙原样保留。
        let pos_c = |m: &LayoutModel| {
            let c = m.layout().screens.iter().find(|s| s.id.device == dev(3)).unwrap();
            (c.rect.x, c.rect.y)
        };
        let before = pos_c(&m);
        // 设备 1 的分辨率变了：其他设备的位置不动。
        m.set_device_displays(dev(1), vec![display("a", 2000.0, 2000.0, (300.0, 300.0))]);
        assert_eq!(pos_c(&m), before);
        // 相同的显示器列表不算变化。
        assert!(!m.set_device_displays(dev(1), vec![display("a", 2000.0, 2000.0, (300.0, 300.0))]));
    }

    #[test]
    fn conversions_match_topology() {
        let mut m = model3();
        m.set_online([dev(1), dev(2), dev(3)].into());
        let q = m.compile(&Default::default());
        let id = ScreenId::new(dev(2), "x");
        assert!((q.reference_per_native_unit(&id).unwrap() - 0.3 / 686.0).abs() < 1e-9);
        let p = ScreenPos::new(id.clone(), Point::new(500.0, 500.0));
        let plane = q.to_plane(&p).unwrap();
        let back = q.from_plane(&id, plane).unwrap();
        assert!((back.x - p.local.x).abs() < 1e-9 && (back.y - p.local.y).abs() < 1e-9);
    }

    /// 非自动模式下只有本机的显示器改动布局：收到的布局原样采用，别的设备的显示器只记下来。
    #[test]
    fn only_local_displays_change_a_shared_layout() {
        let shared = model3().layout().clone();
        let mut m = LayoutModel::from_layout(Layout::default());
        m.set_local(dev(1));
        // 别的设备（3）的分辨率变了：布局数据不动（等设备 3 自己发布）。
        assert!(!m.set_device_displays(dev(3), vec![display("c", 2000.0, 2000.0, (300.0, 300.0))]));
        // 采用收到的布局：设备 3 的新显示器不合进去，结果与收到的完全相同。
        assert!(!m.set_layout(shared.clone()));
        assert_eq!(m.layout(), &shared);
        // 本机（1）的显示器变了：合进去，布局随之改变（调用方发布新版本）。
        assert!(m.set_device_displays(dev(1), vec![display("a", 2000.0, 2000.0, (400.0, 400.0))]));
        assert_ne!(m.layout(), &shared);
        // 收到的布局里还没有本机的屏幕：采用后补上本机的屏幕，报告「与收到的不同」。
        let mut without_me = shared.clone();
        without_me.screens.retain(|s| s.id.device != dev(1));
        assert!(m.set_layout(without_me.clone()));
        assert!(m.layout().screens.iter().any(|s| s.id.device == dev(1)));
    }
}
