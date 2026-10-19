//! 运行时查询：本机坐标与视平面的换算、指针移动一段距离后落在哪里。
//!
//! ## 距离的单位
//! [`Topology::step`] 的位移是「手部 plane」：源机截获的（已由源系统加速的）位移 × 源屏的
//! [`Topology::reference_per_native_unit`]。指针在某块屏幕上实际走过的视平面距离 = 手部 plane × 该屏的
//! [`Topology::plane_gain`]（按屏增益 × 速度模式系数；物理模式、增益 1 时就是 1）。
//! 换屏后剩余的位移按新屏的系数继续走。

use super::portal::{EdgeInfo, Seg, Segment, SegmentKind, compute_side, edge};
use super::{
    EffectiveLayout, EffectiveScreen, EffectiveSettings, Layout, OwnerLayoutPreferences, ScreenId, ScreenPlacement,
    ScreenPos, Side,
};
use oink_foundation::{DeviceId, Point, Rect};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 半开矩形右、下边界内缩的距离（plane）。
const EPS: f64 = 1e-6 / 686.0;
/// 按屏幕比例模式的参考对角线（plane，约 27 寸）。只影响 `reference_per_native_unit` 的绝对值，不影响手感。
pub(super) const PROPORTIONAL_REFERENCE_DIAGONAL: f64 = 1.0;
/// 一次 `step` 最多处理的事件数（穿越 + 撞墙），防止病态布局下死循环。
const MAX_ITERS: usize = 16;
/// 一次 `step` 最多穿越的次数（research：最多连续穿越两次；这里放宽到 4）。
const MAX_CROSSINGS: usize = 4;

/// [`Topology::step`] 的结果。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepResult {
    /// 终点（一定在 `to.screen` 的范围内）。
    pub to: ScreenPos,
    /// 途中依次进入过的屏幕（不含起点屏；最后一个就是终点屏）。
    pub crossed: Vec<ScreenId>,
    /// 途中 x / y 方向是否被墙挡住过。
    pub blocked_x: bool,
    pub blocked_y: bool,
}

/// First portal met by a motion, before stitching across it.
#[derive(Clone, Debug)]
pub struct PortalExit {
    pub from: ScreenPos,
    pub to: ScreenPos,
    pub side: Side,
    pub gap_distance: f64,
    pub along: (f64, f64),
    /// Unconsumed motion in hand reference units.
    pub remaining: (f64, f64),
    pub trigger_zone: bool,
    /// Source-edge interval for a near miss; absent for an ordinary channel.
    pub trigger_along: Option<(f64, f64)>,
}

/// 从 [`Layout`] 编译出的只读查询结构。
///
/// **在线设备**：离线设备的屏幕在逻辑上视为不存在——不是任何传送门的目标，也不挡路（隔着它的两块
/// 屏幕直接相连，穿越时跳过它所占的位置）；它自己的边全是墙。它仍在 [`Topology::screens`] 里
/// （界面照常画在原位置），[`Topology::is_active`] 为 `false`。用 [`Topology::with_online`] 传入
/// 在线设备（包括本机）；[`Topology::new`] 把所有设备都当作在线。
#[derive(Clone, Debug)]
pub struct Topology {
    screens: Vec<EffectiveScreen>,
    index: HashMap<ScreenId, usize>,
    /// 是否参与传送门：启用且所属设备在线。
    active: Vec<bool>,
    /// `edges[i][side.index()]`：每条边的切分；不参与的屏幕整条边都是墙。
    edges: Vec<[Vec<Seg>; 4]>,
    zones: Vec<[Vec<super::trigger::Zone>; 4]>,
    /// 视平面 plane / 手部 plane。
    scale: Vec<f64>,
    settings: EffectiveSettings,
    source_origins: HashMap<DeviceId, Point>,
    source_scales: HashMap<ScreenId, f64>,
    remote_sources: std::collections::HashSet<DeviceId>,
}

impl Topology {
    /// 编译，所有设备都视为在线。无效的放置（尺寸非正、非有限值）被忽略。
    pub fn new(layout: &Layout, owners: &std::collections::BTreeMap<DeviceId, OwnerLayoutPreferences>) -> Self {
        Self::with_online(layout, owners, |_| true)
    }

    /// 编译，只有 `online` 返回 `true` 的设备的屏幕参与传送门。调用方（`core`）在设备连接、断开时
    /// 重新编译；本机要算在线。
    pub fn with_online(
        layout: &Layout,
        owners: &std::collections::BTreeMap<DeviceId, OwnerLayoutPreferences>,
        online: impl Fn(&DeviceId) -> bool,
    ) -> Self {
        Self::from_effective(&layout.effective(owners), online)
    }

    pub fn from_effective(layout: &EffectiveLayout, online: impl Fn(&DeviceId) -> bool) -> Self {
        let screens: Vec<_> = layout.screens().iter().filter(|s| s.is_valid()).cloned().collect();
        let rects: Vec<Rect> = screens.iter().map(|s| s.rect).collect();
        let active: Vec<bool> = screens.iter().map(|s| s.enabled && online(&s.id.device)).collect();
        let st = layout.settings();
        let edges: Vec<[Vec<Seg>; 4]> = (0..screens.len())
            .map(|i| {
                Side::ALL.map(|side| {
                    if active[i] {
                        let segments = compute_side(&rects, &active, i, side, st, |b| {
                            if screens[i].id.device != screens[b].id.device {
                                return Some(st.minimum_portal);
                            }
                            // Same-computer edges come only from native OS adjacency.
                            // Cross-computer positive-gap portals remain layout policy.
                            let a = screens[i].local;
                            let b = screens[b].local;
                            let eps = super::facing::tolerance(&[a, b]);
                            super::facing::facing(a, b, side, eps)
                                .filter(|f| f.gap <= eps)
                                .map(|_| 0.0)
                        });
                        // Cross-computer wall/snap policy cannot extend a native OS seam.
                        segments
                            .into_iter()
                            .flat_map(|segment| {
                                let Some((target, _)) = segment.portal else {
                                    return vec![segment];
                                };
                                if screens[target].id.device != screens[i].id.device {
                                    return vec![segment];
                                }
                                let (_, lo, hi) = edge(&rects[i], side);
                                let (_, blo, bhi) = edge(&rects[target], side.opposite());
                                let from = segment.from.max(lo.max(blo));
                                let to = segment.to.min(hi.min(bhi));
                                let mut pieces = Vec::new();
                                if segment.from < from {
                                    pieces.push(Seg {
                                        from: segment.from,
                                        to: from,
                                        portal: None,
                                    });
                                }
                                if from < to {
                                    pieces.push(Seg {
                                        from,
                                        to,
                                        portal: segment.portal,
                                    });
                                }
                                if to < segment.to {
                                    pieces.push(Seg {
                                        from: to,
                                        to: segment.to,
                                        portal: None,
                                    });
                                }
                                pieces
                            })
                            .collect()
                    } else {
                        let (_, lo, hi) = edge(&rects[i], side);
                        vec![Seg {
                            from: lo,
                            to: hi,
                            portal: None,
                        }]
                    }
                })
            })
            .collect();
        let zones = edges
            .iter()
            .enumerate()
            .map(|(i, edges)| {
                Side::ALL.map(|side| {
                    let mut zones = super::trigger::compile(&edges[side.index()], st.endpoint_reach);
                    zones.retain(|z| screens[z.target].id.device != screens[i].id.device);
                    zones
                })
            })
            .collect();
        let scale = screens.iter().map(super::plane_gain).collect();
        let index = screens.iter().enumerate().map(|(i, s)| (s.id.clone(), i)).collect();
        Self {
            screens,
            index,
            active,
            edges,
            zones,
            scale,
            settings: st.clone(),
            source_origins: layout.source_origins.clone(),
            source_scales: layout.source_scales.clone(),
            remote_sources: layout.remote_sources.clone(),
        }
    }

    /// 这块屏幕是否参与传送门（启用且所属设备在线）。未知屏幕为 `false`。
    pub fn is_active(&self, id: &ScreenId) -> bool {
        self.index.get(id).is_some_and(|&i| self.active[i])
    }

    pub fn screens(&self) -> &[EffectiveScreen] {
        &self.screens
    }

    pub fn screen(&self, id: &ScreenId) -> Option<&EffectiveScreen> {
        self.index.get(id).map(|&i| &self.screens[i])
    }

    pub fn settings(&self) -> &EffectiveSettings {
        &self.settings
    }

    pub fn contains_device(&self, device: DeviceId) -> bool {
        self.screens
            .iter()
            .zip(&self.active)
            .any(|(s, active)| *active && s.id.device == device)
    }
    /// Source redirection is an explicit setting, independent of target geometry.
    pub fn source_requires_target(&self, device: DeviceId) -> bool {
        self.remote_sources.contains(&device)
    }
    /// Stored anchor used only to choose a destination for an input source.
    pub fn source_origin(&self, device: DeviceId) -> Option<Point> {
        self.source_origins.get(&device).copied()
    }
    pub fn source_reference_per_native_unit(&self, id: &ScreenId) -> Option<f64> {
        self.source_scales.get(id).copied()
    }

    /// 本机坐标 → 视平面 plane（不裁剪）。未知屏幕为 `None`。
    pub fn to_plane(&self, pos: &ScreenPos) -> Option<Point> {
        Some(self.screen(&pos.screen)?.local_to_plane(pos.local))
    }

    /// 视平面 plane → 该屏幕的本机坐标，先裁剪到屏幕范围内（结果一定在 `DisplayInfo::bounds` 内）。
    ///
    /// 本机坐标是连续值；平台实现换成整数像素时应向下取整（右、下边界是开区间）。
    pub fn from_plane(&self, screen: &ScreenId, plane: Point) -> Option<Point> {
        let s = self.screen(screen)?;
        Some(to_local_clamped(s, plane))
    }

    /// Extrapolate a plane point into this screen's native coordinates without
    /// clipping. Geometry callers must retain off-screen direction until after
    /// intersection; ordinary pointer placement continues to use `from_plane`.
    pub fn from_plane_unclamped(&self, screen: &ScreenId, plane: Point) -> Option<Point> {
        Some(self.screen(screen)?.plane_to_local(plane))
    }

    /// 手部 plane / 本机坐标单位（含按屏增益与速度模式）。源机把截获的位移乘以它得到 `step` 的输入；
    /// x、y 比例不同时取几何平均。
    pub fn reference_per_native_unit(&self, screen: &ScreenId) -> Option<f64> {
        let &i = self.index.get(screen)?;
        let (sx, sy) = self.screens[i].physical_scale();
        Some((sx * sy).sqrt() / self.scale[i])
    }

    /// 视平面 plane / 手部 plane（按屏增益 × 速度模式系数）。
    pub fn plane_gain(&self, screen: &ScreenId) -> Option<f64> {
        self.index.get(screen).map(|&i| self.scale[i])
    }

    /// 这个点（plane）落在哪块参与的屏幕上。编辑器的命中测试用 [`Layout::screen_at`]（含离线屏幕）。
    pub fn screen_at(&self, plane: Point) -> Option<&ScreenId> {
        self.screens
            .iter()
            .zip(&self.active)
            .find(|(s, a)| **a && s.rect.contains(plane))
            .map(|(s, _)| &s.id)
    }

    /// 所有屏幕所有边的切分（界面绘制传送门、墙）。
    pub fn edges(&self) -> Vec<EdgeInfo> {
        let mut out = Vec::new();
        for (i, s) in self.screens.iter().enumerate() {
            for side in Side::ALL {
                let (at, _, _) = edge(&s.rect, side);
                let segments = self.edges[i][side.index()]
                    .iter()
                    .map(|g| Segment {
                        from: g.from,
                        to: g.to,
                        kind: match g.portal {
                            None => SegmentKind::Wall,
                            Some((b, gap)) => SegmentKind::Portal {
                                to: self.screens[b].id.clone(),
                                gap_distance: gap,
                            },
                        },
                    })
                    .collect();
                out.push(EdgeInfo {
                    screen: s.id.clone(),
                    side,
                    at,
                    segments,
                });
            }
        }
        out
    }

    /// Extra trigger intervals, separate from the unchanged portal/wall geometry.
    pub fn trigger_zones(&self) -> Vec<super::TriggerZone> {
        self.screens
            .iter()
            .enumerate()
            .flat_map(|(i, screen)| {
                Side::ALL.into_iter().flat_map(move |side| {
                    self.zones[i][side.index()].iter().map(move |zone| super::TriggerZone {
                        screen: screen.id.clone(),
                        side,
                        from: zone.from,
                        to: zone.to,
                        target: self.screens[zone.target].id.clone(),
                        entry_along: zone.entry,
                        gap_distance: zone.gap,
                    })
                })
            })
            .collect()
    }

    /// 指针从 `from` 出发移动 `delta`（手部 plane，见模块说明）：穿过传送门、被墙挡住（沿墙滑动）。
    /// 只在角上相接的屏幕之间不能直接斜穿（先沿一条边滑到传送门所在的位置）。
    /// `from` 不在屏幕范围内时先裁剪进去。未知屏幕为 `None`。
    pub fn step(&self, from: &ScreenPos, delta: (f64, f64)) -> Option<StepResult> {
        self.step_filtered(from, delta, |_| true)
    }

    /// Mask already-compiled portals as walls without recompiling or bridging around them.
    pub fn step_filtered(
        &self,
        from: &ScreenPos,
        delta: (f64, f64),
        online: impl Fn(&DeviceId) -> bool,
    ) -> Option<StepResult> {
        let &a = self.index.get(&from.screen)?;
        let s = &self.screens[a];
        let moving = delta.0.is_finite() && delta.1.is_finite() && (delta.0 != 0.0 || delta.1 != 0.0);
        if !moving {
            // 不动：不经过视平面往返，避免浮点漂移。
            return Some(StepResult {
                to: ScreenPos {
                    screen: s.id.clone(),
                    local: clamp_local(&s.local, from.local),
                },
                crossed: Vec::new(),
                blocked_x: false,
                blocked_y: false,
            });
        }
        Some(self.walk(a, s.local_to_plane(from.local), delta, None, &online))
    }

    /// 本机光标在本机屏幕 `from` 上继续移动 `delta_local`（本机坐标单位，系统已处理过加速）时，
    /// 是否会穿过传送门进入另一块屏幕；会则返回落点（可能是另一台设备的屏幕，也可能是本机的
    /// 另一块屏幕——调用方比较设备 ID）。停在本屏（含撞墙）时为 `None`。
    pub fn exit_through_edge(&self, from: &ScreenPos, delta_local: (f64, f64)) -> Option<ScreenPos> {
        self.exit_through_edge_filtered(from, delta_local, |_| true)
    }

    /// Mask already-compiled portals as walls without recompiling or bridging around them.
    pub fn exit_through_edge_filtered(
        &self,
        from: &ScreenPos,
        delta_local: (f64, f64),
        online: impl Fn(&DeviceId) -> bool,
    ) -> Option<ScreenPos> {
        let &a = self.index.get(&from.screen)?;
        let s = &self.screens[a];
        let hand = self.native_motion(a, delta_local);
        let r = self.walk(a, s.local_to_plane(from.local), hand, None, &online);
        (r.to.screen != from.screen).then_some(r.to)
    }

    /// The first selected portal for native desktop motion. Share the exact
    /// per-axis native conversion used by exit_through_edge_filtered.
    pub fn first_portal_native_filtered(
        &self,
        from: &ScreenPos,
        delta_local: (f64, f64),
        online: impl Fn(&DeviceId) -> bool,
    ) -> Option<PortalExit> {
        let &a = self.index.get(&from.screen)?;
        let hand = self.native_motion(a, delta_local);
        self.first_portal_filtered(from, hand, online)
    }

    fn native_motion(&self, index: usize, delta: (f64, f64)) -> (f64, f64) {
        let (sx, sy) = self.screens[index].physical_scale();
        let k = self.scale[index];
        (delta.0 * sx / k, delta.1 * sy / k)
    }

    /// Uses the same wall sliding, corner priority and portal selection as `step`.
    pub fn first_portal(&self, from: &ScreenPos, delta: (f64, f64)) -> Option<PortalExit> {
        self.first_portal_filtered(from, delta, |_| true)
    }

    /// Mask already-compiled portals as walls without recompiling or bridging around them.
    pub fn first_portal_filtered(
        &self,
        from: &ScreenPos,
        delta: (f64, f64),
        online: impl Fn(&DeviceId) -> bool,
    ) -> Option<PortalExit> {
        let &a = self.index.get(&from.screen)?;
        let mut found = None;
        self.walk(
            a,
            self.screens[a].local_to_plane(from.local),
            delta,
            Some(&mut found),
            &online,
        );
        found
    }

    fn walk(
        &self,
        start: usize,
        p0: Point,
        delta: (f64, f64),
        first: Option<&mut Option<PortalExit>>,
        online: &dyn Fn(&DeviceId) -> bool,
    ) -> StepResult {
        let mut a = start;
        let mut p = clamp_in(&self.screens[a].rect, p0);
        let mut rem = if delta.0.is_finite() && delta.1.is_finite() {
            delta
        } else {
            (0.0, 0.0)
        };
        let mut crossed = Vec::new();
        let (mut blocked_x, mut blocked_y) = (false, false);

        for _ in 0..MAX_ITERS {
            if rem.0 == 0.0 && rem.1 == 0.0 {
                break;
            }
            let r = self.screens[a].rect;
            let k = self.scale[a];
            let d = (rem.0 * k, rem.1 * k);
            // 离开半开矩形 [x, right) × [y, bottom) 的参数：正方向到达右/下边界即离开，
            // 负方向越过左/上边界才离开。
            let (tx, ex) = exit_param(p.x, d.0, r.x, r.right());
            let (ty, ey) = exit_param(p.y, d.1, r.y, r.bottom());
            if !ex && !ey {
                p = clamp_in(&r, Point::new(p.x + d.0, p.y + d.1));
                break;
            }
            let t = match (ex, ey) {
                (true, true) => tx.min(ty),
                (true, false) => tx,
                _ => ty,
            }
            .max(0.0);
            let side_x = if d.0 > 0.0 { Side::Right } else { Side::Left };
            let side_y = if d.1 > 0.0 { Side::Bottom } else { Side::Top };
            // 同时到达两条边（角落）时两条都要看；先 x 后 y。
            let tie = 1e-12_f64.max(t.abs() * 1e-12);
            let mut sides: Vec<Side> = Vec::with_capacity(2);
            if ex && (tx - t).abs() <= tie {
                sides.push(side_x);
            }
            if ey && (ty - t).abs() <= tie {
                sides.push(side_y);
            }
            let mut hit = Point::new(p.x + d.0 * t, p.y + d.1 * t);
            for &s in &sides {
                // 消除浮点漂移：命中点正好落在边上。
                let (at, _, _) = edge(&r, s);
                if s.is_vertical() {
                    hit.x = at;
                } else {
                    hit.y = at;
                }
            }
            let left = 1.0 - t;
            let rem_after = (rem.0 * left, rem.1 * left);

            // 传送门？
            let portal = sides
                .iter()
                .find_map(|&s| {
                    let along = if s.is_vertical() { hit.y } else { hit.x };
                    self.portal_at(a, s, along)
                        .filter(|b| online(&self.screens[a].id.device) && online(&self.screens[*b].id.device))
                        .map(|b| (b, s, along, None))
                })
                .or_else(|| {
                    sides.iter().find_map(|&s| {
                        let along = if s.is_vertical() { hit.y } else { hit.x };
                        // Channels own their endpoints even while masked offline.
                        if self.portal_at(a, s, along).is_some() {
                            return None;
                        }
                        self.zones[a][s.index()]
                            .iter()
                            .find(|z| z.from - EPS <= along && along <= z.to + EPS)
                            .filter(|z| online(&self.screens[a].id.device) && online(&self.screens[z.target].id.device))
                            .map(|z| (z.target, s, z.entry, Some(z)))
                    })
                });
            if let Some((b, s, along, zone)) = portal {
                if let Some(first) = first {
                    let (gap, channel) = if let Some(z) = zone {
                        (z.gap, z.channel)
                    } else {
                        let seg = self.edges[a][s.index()]
                            .iter()
                            .find(|g| g.from <= along && along <= g.to && g.portal.is_some())
                            .unwrap();
                        (seg.portal.unwrap().1, (seg.from, seg.to))
                    };
                    *first = Some(PortalExit {
                        from: ScreenPos {
                            screen: self.screens[a].id.clone(),
                            local: to_local_clamped(&self.screens[a], hit),
                        },
                        to: ScreenPos {
                            screen: self.screens[b].id.clone(),
                            local: to_local_clamped(&self.screens[b], entry_point(&self.screens[b].rect, s, along)),
                        },
                        side: s,
                        gap_distance: gap,
                        along: channel,
                        remaining: rem_after,
                        trigger_zone: zone.is_some(),
                        trigger_along: zone.map(|z| (z.from, z.to)),
                    });
                    p = hit;
                    break;
                }
                if crossed.len() >= MAX_CROSSINGS {
                    p = hit;
                    break;
                }
                p = entry_point(&self.screens[b].rect, s, along);
                a = b;
                crossed.push(b);
                rem = rem_after;
                continue;
            }
            // 墙：挡住垂直于墙的分量，剩余位移沿墙滑动。
            p = hit;
            rem = rem_after;
            for &s in &sides {
                if s.is_vertical() {
                    rem.0 = 0.0;
                    blocked_x = true;
                } else {
                    rem.1 = 0.0;
                    blocked_y = true;
                }
            }
            p = clamp_in(&r, p);
        }

        let s = &self.screens[a];
        let p = clamp_in(&s.rect, p);
        StepResult {
            to: ScreenPos {
                screen: s.id.clone(),
                local: to_local_clamped(s, p),
            },
            crossed: crossed.into_iter().map(|i| self.screens[i].id.clone()).collect(),
            blocked_x,
            blocked_y,
        }
    }

    /// 屏幕 `a` 的 `side` 边在沿边坐标 `along` 处是否是传送门，是则返回目标屏下标。
    fn portal_at(&self, a: usize, side: Side, along: f64) -> Option<usize> {
        let segs = &self.edges[a][side.index()];
        let (_, lo, hi) = edge(&self.screens[a].rect, side);
        let along = along.clamp(lo, hi);
        segs.iter()
            .find(|g| g.portal.is_some() && g.from <= along && along <= g.to)
            .and_then(|g| g.portal.map(|(b, _)| b))
    }
}

/// 一维：从 `p` 以速度 `d` 移动一个单位时间，离开 `[lo, hi)` 的参数 `t` 以及是否在本步内离开。
fn exit_param(p: f64, d: f64, lo: f64, hi: f64) -> (f64, bool) {
    if d > 0.0 {
        let t = (hi - p) / d;
        (t, t <= 1.0)
    } else if d < 0.0 {
        let t = (lo - p) / d;
        (t, t < 1.0)
    } else {
        (f64::INFINITY, false)
    }
}

/// 从 A 的 `side` 边沿边坐标 `along` 处穿过传送门后，在目标矩形上的落点（stitching：沿边位置不变，
/// 超出目标边范围时贴到端点；垂直方向跳过间隙，落在目标的相对边上）。
fn entry_point(rb: &Rect, side: Side, along: f64) -> Point {
    let ay = along.clamp(rb.y, rb.bottom() - EPS);
    let ax = along.clamp(rb.x, rb.right() - EPS);
    match side {
        Side::Right => Point::new(rb.x, ay),
        Side::Left => Point::new(rb.right() - EPS, ay),
        Side::Bottom => Point::new(ax, rb.y),
        Side::Top => Point::new(ax, rb.bottom() - EPS),
    }
}

/// 把点限制在半开矩形 `[x, right) × [y, bottom)` 内。
fn clamp_in(r: &Rect, p: Point) -> Point {
    let x = if p.x.is_finite() { p.x } else { r.x };
    let y = if p.y.is_finite() { p.y } else { r.y };
    Point::new(
        x.clamp(r.x, (r.right() - EPS).max(r.x)),
        y.clamp(r.y, (r.bottom() - EPS).max(r.y)),
    )
}

fn to_local_clamped(s: &ScreenPlacement, plane: Point) -> Point {
    // 视平面的 EPS 换算到本机坐标后可能被舍入到边界上，再裁剪一次。
    clamp_local(&s.local, s.plane_to_local(clamp_in(&s.rect, plane)))
}

/// 把本机坐标限制在半开矩形内（已在内部的点原样返回）。
fn clamp_local(r: &Rect, l: Point) -> Point {
    let e = (r.w.abs().max(r.h.abs()) * 1e-12).max(1e-9);
    let x = if l.x.is_finite() { l.x } else { r.x };
    let y = if l.y.is_finite() { l.y } else { r.y };
    Point::new(
        x.clamp(r.x, (r.right() - e).max(r.x)),
        y.clamp(r.y, (r.bottom() - e).max(r.y)),
    )
}
