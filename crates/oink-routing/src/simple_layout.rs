//! [`LayoutQuery`] 的简单实现：每块屏幕是视平面上的一个轴对齐矩形，本机坐标到视平面是缩放 + 平移。
//!
//! 供多设备模拟器、单元测试和本机回环自测使用；正式实现（标定得到的单应、传送门表）由
//! `oink_display::layout` supplies the full implementation. Simplified rules:
//! - 屏幕 A 的一条边与屏幕 B 的对边间隙在 `[minimum_gap, maximum_gap]` 内、沿边方向有重叠，即为传送门；
//! - 映射方式为 stitching：跳过间隙，沿边位置不变；
//! - 端点吸附（`portal_snap`）：传送门端点离边的端点很近时延伸到端点；
//! - 桥接（`bridge_reach`）：两个传送门之间的窄墙从中间劈开；
//! - 其余为墙：指针沿墙滑动。

use oink_foundation::display::DisplayInfo;
use oink_foundation::{DeviceId, Point, Rect};

use crate::layout::{LayoutQuery, ScreenId, ScreenPos, StepResult};

const EPS: f64 = 1e-6 / 686.0;

#[derive(Clone, Debug, PartialEq)]
pub struct SimpleScreen {
    pub id: ScreenId,
    /// 本机全局坐标中的矩形（`DisplayInfo::bounds`）。
    pub local: Rect,
    /// 视平面中的矩形（plane）。
    pub plane: Rect,
}

impl SimpleScreen {
    fn scale(&self) -> f64 {
        if self.local.w > 0.0 {
            self.plane.w / self.local.w
        } else {
            1.0
        }
    }

    fn local_to_plane(&self, p: Point) -> Point {
        let sx = self.plane.w / self.local.w;
        let sy = self.plane.h / self.local.h;
        Point::new(
            self.plane.x + (p.x - self.local.x) * sx,
            self.plane.y + (p.y - self.local.y) * sy,
        )
    }

    fn plane_to_local(&self, q: Point) -> Point {
        let sx = self.local.w / self.plane.w;
        let sy = self.local.h / self.plane.h;
        Point::new(
            self.local.x + (q.x - self.plane.x) * sx,
            self.local.y + (q.y - self.plane.y) * sy,
        )
    }
}

/// 传送门候选：(屏幕下标, 间隙 plane, 沿边区间起点, 终点)。
type Cand = (usize, f64, f64, f64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug)]
pub struct SimpleLayout {
    screens: Vec<SimpleScreen>,
    /// 传送门允许的最小/最大间隙（plane）。负值表示允许少量重叠。
    pub minimum_gap: f64,
    pub maximum_gap: f64,
    /// 桥接：两个传送门之间不超过这么宽（plane）的墙从中间劈开，分给两侧。
    pub bridge_reach: f64,
    /// 吸附：传送门端点离本边端点不到这么远（plane）时延伸到端点。
    pub portal_snap: f64,
}

impl Default for SimpleLayout {
    fn default() -> Self {
        Self {
            screens: Vec::new(),
            minimum_gap: -5.0 / 686.0,
            maximum_gap: 60.0 / 686.0,
            bridge_reach: 40.0 / 686.0,
            portal_snap: 8.0 / 686.0,
        }
    }
}

impl SimpleLayout {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_screen(&mut self, id: ScreenId, local: Rect, plane: Rect) -> &mut Self {
        self.screens.retain(|s| s.id != id);
        self.screens.push(SimpleScreen { id, local, plane });
        self
    }

    /// 按显示器的物理尺寸把一台设备的屏幕放到视平面上：本机坐标原点对应 `origin`，
    /// 各屏按各自的 reference_per_native_unit 缩放（物理尺寸未知时按 `fallback_reference_per_native_unit`）。
    pub fn add_device_displays(
        &mut self,
        device: DeviceId,
        displays: &[DisplayInfo],
        origin: Point,
        fallback_reference_per_native_unit: f64,
    ) -> &mut Self {
        for d in displays {
            let s = d
                .reference_per_native_unit()
                .unwrap_or(fallback_reference_per_native_unit);
            let plane = Rect::new(
                origin.x + d.bounds.x * s,
                origin.y + d.bounds.y * s,
                d.bounds.w * s,
                d.bounds.h * s,
            );
            self.add_screen(ScreenId::new(device, d.key.clone()), d.bounds, plane);
        }
        self
    }

    pub fn screens(&self) -> &[SimpleScreen] {
        &self.screens
    }

    fn find(&self, id: &ScreenId) -> Option<usize> {
        self.screens.iter().position(|s| &s.id == id)
    }

    /// 从屏幕 `from` 的 `side` 边、沿边坐标 `along` 处出去，进入哪块屏幕的哪个点。
    fn portal(&self, from: usize, side: Side, along: f64) -> Option<(usize, Point)> {
        let a = &self.screens[from].plane;
        let (edge_lo, edge_hi) = match side {
            Side::Left | Side::Right => (a.y, a.bottom()),
            Side::Top | Side::Bottom => (a.x, a.right()),
        };
        // 候选：间隙在范围内、沿边方向与本边有重叠的屏幕。(下标, 间隙, 沿边区间)
        let cands: Vec<Cand> = self
            .screens
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != from)
            .filter_map(|(i, b)| {
                let p = &b.plane;
                let (gap, lo, hi) = match side {
                    Side::Right => (p.x - a.right(), p.y, p.bottom()),
                    Side::Left => (a.x - p.right(), p.y, p.bottom()),
                    Side::Bottom => (p.y - a.bottom(), p.x, p.right()),
                    Side::Top => (a.y - p.bottom(), p.x, p.right()),
                };
                let ok = gap >= self.minimum_gap && gap <= self.maximum_gap && hi > edge_lo && lo < edge_hi;
                ok.then_some((i, gap, lo, hi))
            })
            .collect();
        let nearest = |it: &mut dyn Iterator<Item = &Cand>, key: &dyn Fn(&Cand) -> f64| {
            it.min_by(|x, y| key(x).total_cmp(&key(y))).copied()
        };
        // 1. 正对着的传送门：取间隙最小的。
        let facing = nearest(&mut cands.iter().filter(|c| c.2 <= along && along < c.3), &|c| c.1);
        // 2. 吸附：传送门端点离本边端点不到 portal_snap 时延伸到端点。
        let snapped = || {
            nearest(
                &mut cands.iter().filter(|c| {
                    (along < c.2 && c.2 - edge_lo <= self.portal_snap)
                        || (along >= c.3 && edge_hi - c.3 <= self.portal_snap)
                }),
                &|c| c.1,
            )
        };
        // 3. 桥接：两个传送门之间不超过 bridge_reach 的墙从中间劈开。
        let bridged = || {
            let below = nearest(&mut cands.iter().filter(|c| c.3 <= along), &|c| along - c.3);
            let above = nearest(&mut cands.iter().filter(|c| c.2 > along), &|c| c.2 - along);
            match (below, above) {
                (Some(b), Some(u)) if u.2 - b.3 <= self.bridge_reach => {
                    Some(if along - b.3 <= u.2 - along { b } else { u })
                }
                _ => None,
            }
        };
        let (i, _, lo, hi) = facing.or_else(snapped).or_else(bridged)?;
        let p = &self.screens[i].plane;
        let t = along.clamp(lo, hi - EPS);
        let entry = match side {
            Side::Right => Point::new(p.x, t),
            Side::Left => Point::new(p.right() - EPS, t),
            Side::Bottom => Point::new(t, p.y),
            Side::Top => Point::new(t, p.bottom() - EPS),
        };
        Some((i, entry))
    }

    fn step_idx(&self, mut cur: usize, mut p: Point, delta: (f64, f64)) -> (usize, Point, Vec<ScreenId>, bool, bool) {
        let (mut dx, mut dy) = delta;
        let mut crossed = Vec::new();
        let (mut bx, mut by) = (false, false);
        for _ in 0..6 {
            let r = self.screens[cur].plane;
            let q = Point::new(p.x + dx, p.y + dy);
            if r.contains(q) {
                return (cur, q, crossed, bx, by);
            }
            // 最早离开矩形的边（Liang–Barsky 的简化：p 在矩形内）。
            let tx = if dx > 0.0 {
                Some(((r.right() - p.x) / dx, Side::Right))
            } else if dx < 0.0 {
                Some(((r.x - p.x) / dx, Side::Left))
            } else {
                None
            };
            let ty = if dy > 0.0 {
                Some(((r.bottom() - p.y) / dy, Side::Bottom))
            } else if dy < 0.0 {
                Some(((r.y - p.y) / dy, Side::Top))
            } else {
                None
            };
            let (t, side) = match (tx, ty) {
                (Some(a), Some(b)) => {
                    if a.0 <= b.0 {
                        a
                    } else {
                        b
                    }
                }
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => return (cur, r.clamp(p), crossed, bx, by),
            };
            let t = t.clamp(0.0, 1.0);
            let e = Point::new(p.x + dx * t, p.y + dy * t);
            let (rdx, rdy) = (dx * (1.0 - t), dy * (1.0 - t));
            let along = match side {
                Side::Left | Side::Right => e.y,
                Side::Top | Side::Bottom => e.x,
            };
            match self.portal(cur, side, along) {
                Some((next, entry)) => {
                    crossed.push(self.screens[next].id.clone());
                    cur = next;
                    p = entry;
                    dx = rdx;
                    dy = rdy;
                }
                None => {
                    // 墙：挡住这个方向，另一个方向继续（沿墙滑动）。
                    p = r.clamp(e);
                    match side {
                        Side::Left | Side::Right => {
                            bx = true;
                            dx = 0.0;
                            dy = rdy;
                        }
                        Side::Top | Side::Bottom => {
                            by = true;
                            dx = rdx;
                            dy = 0.0;
                        }
                    }
                }
            }
        }
        let r = self.screens[cur].plane;
        (cur, r.clamp(Point::new(p.x + dx, p.y + dy)), crossed, bx, by)
    }
}

impl LayoutQuery for SimpleLayout {
    fn focus_candidates(&self, from: DeviceId) -> Vec<ScreenPos> {
        let mut screens: Vec<_> = self.screens.iter().filter(|screen| screen.id.device != from).collect();
        screens.sort_by(|a, b| a.id.device.cmp(&b.id.device).then(a.id.display.cmp(&b.id.display)));
        screens
            .into_iter()
            .map(|screen| ScreenPos::new(screen.id.clone(), screen.local.center()))
            .collect()
    }

    fn contains_device(&self, device: DeviceId) -> bool {
        self.screens.iter().any(|s| s.id.device == device)
    }
    fn to_plane(&self, pos: &ScreenPos) -> Option<Point> {
        self.find(&pos.screen)
            .map(|i| self.screens[i].local_to_plane(pos.local))
    }

    fn from_plane(&self, screen: &ScreenId, plane: Point) -> Option<Point> {
        self.find(screen).map(|i| {
            let screen = &self.screens[i];
            let plane = Point::new(
                if plane.x.is_finite() { plane.x } else { screen.plane.x },
                if plane.y.is_finite() { plane.y } else { screen.plane.y },
            );
            screen.local.clamp(screen.plane_to_local(screen.plane.clamp(plane)))
        })
    }

    fn from_plane_unclamped(&self, screen: &ScreenId, plane: Point) -> Option<Point> {
        self.find(screen).map(|i| self.screens[i].plane_to_local(plane))
    }

    fn step(&self, from: &ScreenPos, delta: (f64, f64)) -> StepResult {
        let Some(i) = self.find(&from.screen) else {
            return StepResult {
                to: from.clone(),
                crossed: Vec::new(),
                blocked_x: true,
                blocked_y: true,
            };
        };
        let s = &self.screens[i];
        let p = s.plane.clamp(s.local_to_plane(from.local));
        let (j, q, crossed, blocked_x, blocked_y) = self.step_idx(i, p, delta);
        let t = &self.screens[j];
        let local = t.local.clamp(t.plane_to_local(q));
        StepResult {
            to: ScreenPos::new(t.id.clone(), local),
            crossed,
            blocked_x,
            blocked_y,
        }
    }

    fn exit_through_edge(&self, from: &ScreenPos, delta_local: (f64, f64)) -> Option<ScreenPos> {
        let s = self.reference_per_native_unit(&from.screen)?;
        let r = self.step(from, (delta_local.0 * s, delta_local.1 * s));
        (r.to.screen != from.screen).then_some(r.to)
    }

    fn reference_per_native_unit(&self, screen: &ScreenId) -> Option<f64> {
        self.find(screen).map(|i| self.screens[i].scale())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(n: u8) -> DeviceId {
        DeviceId([n; 32])
    }

    /// Example: 上方 600/686 reference units 宽的屏幕，下方两块 250/686 reference units 宽的屏幕居中放置、间隔 20/686 reference units。
    fn three() -> SimpleLayout {
        let mut l = SimpleLayout::new();
        l.add_screen(
            ScreenId::new(dev(1), "top"),
            Rect::new(0.0, 0.0, 2400.0, 1000.0),
            Rect::new(0.0 / 686.0, 0.0 / 686.0, 600.0 / 686.0, 250.0 / 686.0),
        );
        l.add_screen(
            ScreenId::new(dev(2), "bl"),
            Rect::new(0.0, 0.0, 1000.0, 600.0),
            Rect::new(40.0 / 686.0, 260.0 / 686.0, 250.0 / 686.0, 150.0 / 686.0),
        );
        l.add_screen(
            ScreenId::new(dev(3), "br"),
            Rect::new(0.0, 0.0, 1000.0, 600.0),
            Rect::new(310.0 / 686.0, 260.0 / 686.0, 250.0 / 686.0, 150.0 / 686.0),
        );
        l
    }

    #[test]
    fn stitching_keeps_along_position() {
        let l = three();
        // 顶屏 x = 100/686 reference units（本机 400）处向下出去 → 左下屏顶边 60/686 reference units 处（本机 240）。
        let from = ScreenPos::new(ScreenId::new(dev(1), "top"), Point::new(400.0, 996.0));
        let r = l.step(&from, (0.0, 5.0 / 686.0));
        assert_eq!(r.to.screen, ScreenId::new(dev(2), "bl"));
        assert!((r.to.local.x - 240.0).abs() < 1e-6);
        assert!(r.to.local.y > 0.0 && r.to.local.y < 20.0);
    }

    #[test]
    fn wall_slides() {
        let l = three();
        // x = 20/686 reference units 处是墙（离左下屏 20/686 reference units，恰在桥接边界之外一点）
        let from = ScreenPos::new(ScreenId::new(dev(1), "top"), Point::new(40.0, 990.0));
        let r = l.step(&from, (10.0 / 686.0, 20.0 / 686.0));
        assert_eq!(r.to.screen.device, dev(1));
        assert!(r.blocked_y);
        assert!(r.to.local.x > 40.0);
    }

    #[test]
    fn bridge_splits_small_gap() {
        let l = three();
        // x = 298/686 reference units：在两块下屏之间 20/686 reference units 的缝里，靠左 → 左下屏右端；x = 302 → 右下屏左端
        let top = ScreenId::new(dev(1), "top");
        let r = l.step(
            &ScreenPos::new(top.clone(), Point::new(298.0 * 4.0, 999.0)),
            (0.0, 5.0 / 686.0),
        );
        assert_eq!(r.to.screen.device, dev(2));
        let r = l.step(&ScreenPos::new(top, Point::new(302.0 * 4.0, 999.0)), (0.0, 5.0 / 686.0));
        assert_eq!(r.to.screen.device, dev(3));
    }

    #[test]
    fn exit_through_edge_reports_other_screens() {
        let l = three();
        let top = ScreenId::new(dev(1), "top");
        assert!(
            l.exit_through_edge(&ScreenPos::new(top.clone(), Point::new(400.0, 500.0)), (0.0, 5.0))
                .is_none()
        );
        let hit = l.exit_through_edge(&ScreenPos::new(top, Point::new(400.0, 999.9)), (0.0, 5.0));
        assert_eq!(hit.unwrap().screen.device, dev(2));
    }

    #[test]
    fn roundtrip_plane() {
        let l = three();
        let p = ScreenPos::new(ScreenId::new(dev(2), "bl"), Point::new(500.0, 300.0));
        let plane = l.to_plane(&p).unwrap();
        assert!((plane.x - 165.0 / 686.0).abs() < 1e-12 && (plane.y - 335.0 / 686.0).abs() < 1e-12);
        let back = l.from_plane(&p.screen, plane).unwrap();
        assert!((back.x - p.local.x).abs() < 1e-9 && (back.y - p.local.y).abs() < 1e-9);
    }
}
