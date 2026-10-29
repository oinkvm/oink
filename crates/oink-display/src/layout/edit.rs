//! Rigid computer editing and overlap repair.

use super::{
    Layout, ScreenId, SizeSource,
    facing::{overlaps, tolerance},
};
use oink_foundation::{DeviceId, Point, Rect, Size};

impl Layout {
    /// Newly attached screens pin their owner. Only computers intersecting a new
    /// screen in the initial import may move; siblings follow their rigid unit.
    /// Visit colliders by device identity, choosing the nearest free translation
    /// against every other actual screen (including still-unrepaired colliders).
    /// Ties use dx, then dy. No cascading pushes or attachment compaction.
    pub fn repair_new_screens(&mut self, owner: DeviceId, added: &[ScreenId]) {
        let new = self
            .screens
            .iter()
            .filter(|s| added.contains(&s.id) && s.is_valid())
            .map(|s| s.rect)
            .collect::<Vec<_>>();
        let colliders = self
            .screens
            .iter()
            .filter(|s| {
                s.id.device != owner
                    && s.is_valid()
                    && new.iter().any(|r| overlaps(s.rect, *r, tolerance(&[s.rect, *r])))
            })
            .map(|s| s.id.device)
            .collect::<std::collections::BTreeSet<_>>();
        for device in colliders {
            self.repair_native_change(device);
        }
    }

    /// Translate an overlapping native desktop to its nearest free position.
    /// Preserve its native geometry/scale and every other computer. Native
    /// changes never restore lost contact: a gap is not a collision.
    pub fn repair_native_change(&mut self, device: DeviceId) {
        let group = self
            .screens
            .iter()
            .filter(|s| s.id.device == device && s.is_valid())
            .map(|s| s.rect)
            .collect::<Vec<_>>();
        if group.is_empty() {
            return;
        }
        let obstacles = self
            .screens
            .iter()
            .filter(|s| s.id.device != device && s.is_valid())
            .map(|s| s.rect)
            .collect::<Vec<_>>();
        let moved = |r: Rect, x, y| Rect::new(r.x + x, r.y + y, r.w, r.h);
        let fits = |x, y| {
            group.iter().all(|a| {
                obstacles.iter().all(|b| {
                    let a = moved(*a, x, y);
                    !overlaps(a, *b, tolerance(&[a, *b]))
                })
            })
        };
        if fits(0.0, 0.0) {
            return;
        }
        // The feasible translation region is bounded by actual screen edges.
        // Include both axes' edge differences: a nearest free position may be
        // diagonal and constrained by two different stationary computers.
        let mut xs = vec![0.0];
        let mut ys = vec![0.0];
        for a in &group {
            for b in &obstacles {
                xs.extend([b.x - a.right(), b.right() - a.x]);
                ys.extend([b.y - a.bottom(), b.bottom() - a.y]);
            }
        }
        xs.sort_by(f64::total_cmp);
        xs.dedup();
        ys.sort_by(f64::total_cmp);
        ys.dedup();
        let mut best: Option<(f64, f64, f64)> = None;
        for x in xs {
            for &y in &ys {
                let cost = x * x + y * y;
                if best.is_some_and(|(c, _, _)| cost >= c) || !fits(x, y) {
                    continue;
                }
                best = Some((cost, x, y));
            }
        }
        if let Some((_, dx, dy)) = best {
            for s in self.screens.iter_mut().filter(|s| s.id.device == device) {
                s.translate(dx, dy);
            }
        }
    }

    /// Separate actual inter-computer collisions without closing intentional gaps.
    pub fn repair_overlaps(&mut self) {
        self.normalize_pinned(None);
    }

    /// Repair collisions while keeping the specified screen anchored. Explicit
    /// API resizes use this; native observations use `repair_native_change`.
    /// Contact compaction belongs to the canvas editor alone.
    pub fn repair_overlaps_around(&mut self, id: &ScreenId) {
        let pin = self.screens.iter().position(|s| &s.id == id);
        self.normalize_pinned(pin);
    }

    /// 规整，整体平移使屏幕 `pin`（下标；`None` 为第一块有效屏幕）的位置不变。
    fn normalize_pinned(&mut self, pin: Option<usize>) {
        let mut devices = self
            .screens
            .iter()
            .filter(|s| s.is_valid())
            .map(|s| s.id.device)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let anchor = pin
            .and_then(|i| self.screens.get(i))
            .or_else(|| self.screens.first())
            .map(|s| s.id.device);
        if let Some(index) = devices.iter().position(|d| Some(*d) == anchor) {
            devices.swap(0, index);
        }
        let mut placed: Vec<Rect> = Vec::new();
        for device in devices {
            let group = self
                .screens
                .iter()
                .filter(|s| s.id.device == device && s.is_valid())
                .map(|s| s.rect)
                .collect::<Vec<_>>();
            let fits = |dx: f64, dy: f64| {
                group.iter().all(|g| {
                    placed
                        .iter()
                        .all(|o| !overlaps(Rect::new(g.x + dx, g.y + dy, g.w, g.h), *o, tolerance(&[*g, *o])))
                })
            };
            let (mut dx, mut dy) = (0.0, 0.0);
            if !fits(0.0, 0.0) {
                let mut xs = vec![0.0];
                let mut ys = vec![0.0];
                for g in &group {
                    for o in &placed {
                        xs.extend([o.x - g.right(), o.right() - g.x]);
                        ys.extend([o.y - g.bottom(), o.bottom() - g.y]);
                    }
                }
                let mut best = f64::INFINITY;
                for x in xs {
                    for &y in &ys {
                        let cost = x * x + y * y;
                        if cost < best && fits(x, y) {
                            dx = x;
                            dy = y;
                            best = cost;
                        }
                    }
                }
            }
            for s in self.screens.iter_mut().filter(|s| s.id.device == device) {
                s.translate(dx, dy);
                if s.is_valid() {
                    placed.push(s.rect);
                }
            }
        }
    }

    /// Native desktop origin and canvas units per native unit. Screen rectangles
    /// are an expanded runtime view; the shared register stores this transform once.
    pub fn device_transform(&self, device: DeviceId) -> Option<(Point, f64)> {
        let s = self.screens.iter().find(|s| s.id.device == device)?;
        let k = s.rect.w / s.local.w;
        Some((Point::new(s.rect.x - s.local.x * k, s.rect.y - s.local.y * k), k))
    }

    pub fn device_bounds(&self, device: DeviceId) -> Option<Rect> {
        super::bbox(self.screens.iter().filter(|s| s.id.device == device).map(|s| s.rect))
    }

    /// Apply a group transform to every screen, including disabled screens.
    pub fn place_device(&mut self, device: DeviceId, origin: Point, scale: f64, manual: bool) -> bool {
        if !scale.is_finite() || scale <= 0.0 || !origin.x.is_finite() || !origin.y.is_finite() {
            return false;
        }
        let mut found = false;
        for s in self.screens.iter_mut().filter(|s| s.id.device == device) {
            s.rect = Rect::new(
                origin.x + s.local.x * scale,
                origin.y + s.local.y * scale,
                s.local.w * scale,
                s.local.h * scale,
            );
            if manual {
                s.size_source = SizeSource::Manual;
            }
            found = true;
        }
        found
    }

    /// 这个点（plane）落在哪块屏幕上（编辑器命中测试；含未启用、离线设备的屏幕）。
    pub fn screen_at(&self, plane: Point) -> Option<&ScreenId> {
        self.screens.iter().find(|s| s.rect.contains(plane)).map(|s| &s.id)
    }

    /// 拖动预览：合法位置原样保留；重叠时取最近的合法边界位置（左上角）。
    pub fn drop_position(&self, id: &ScreenId, to: Point) -> Option<Point> {
        let i = self.screens.iter().position(|s| &s.id == id)?;
        let r = self.screens[i].rect;
        let members: Vec<_> = (0..self.screens.len())
            .filter(|&j| self.screens[j].id.device == id.device)
            .collect();
        let d = self.resolve_drag(&members, Point::new(to.x - r.x, to.y - r.y));
        Some(Point::new(r.x + d.x, r.y + d.y))
    }

    /// 拖动预览（整个布局）：屏幕 `id` 的左上角拖到 `to` 并放下后的逻辑布局。不修改 `self`。
    pub fn preview_move(&self, id: &ScreenId, to: Point) -> Option<Layout> {
        let mut l = self.clone();
        l.move_screen(id, to).then_some(l)
    }

    /// 自由放下屏幕。实际重叠时沿最近的合法边界推出；无合法候选时回到原位置。
    /// 其他屏幕不动，间隙不收拢。找不到该屏幕时返回 `false`。
    pub fn move_screen(&mut self, id: &ScreenId, to: Point) -> bool {
        let Some(i) = self.screens.iter().position(|s| &s.id == id) else {
            return false;
        };
        let r = self.screens[i].rect;
        self.move_device(id.device, Point::new(to.x - r.x, to.y - r.y));
        true
    }

    /// 整台设备的所有屏幕一起平移 `delta` 后放下（规则同 [`Layout::move_screen`]，设备内部的相对
    /// 位置不变）。该设备没有屏幕时返回 `false`。
    pub fn move_device(&mut self, device: DeviceId, delta: Point) -> bool {
        let members: Vec<usize> = (0..self.screens.len())
            .filter(|&i| self.screens[i].id.device == device)
            .collect();
        if members.is_empty() {
            return false;
        }
        let d = self.resolve_drag(&members, delta);
        for &i in &members {
            self.screens[i].translate(d.x, d.y);
        }
        true
    }

    /// 拖动预览：整台设备平移 `delta` 并放下后的逻辑布局。
    pub fn preview_move_device(&self, device: DeviceId, delta: Point) -> Option<Layout> {
        let mut l = self.clone();
        l.move_device(device, delta).then_some(l)
    }

    /// 一组屏幕拟平移 `delta`：无重叠时原样返回；否则只尝试最近的合法接触边界。
    /// 沿另一方向不做吸附，也不要求形成传送门。无合法候选时拒绝移动。
    fn resolve_drag(&self, members: &[usize], delta: Point) -> Point {
        if !delta.x.is_finite() || !delta.y.is_finite() {
            return Point::default();
        }
        let others: Vec<Rect> = self
            .screens
            .iter()
            .enumerate()
            .filter(|(i, s)| !members.contains(i) && s.is_valid())
            .map(|(_, s)| s.rect)
            .collect();
        let group: Vec<Rect> = members
            .iter()
            .filter_map(|&i| self.screens[i].is_valid().then_some(self.screens[i].rect))
            .collect();
        let fits = |d: Point| {
            group.iter().all(|g| {
                let moved = Rect::new(g.x + d.x, g.y + d.y, g.w, g.h);
                others.iter().all(|o| !overlaps(moved, *o, tolerance(&[moved, *o])))
            })
        };
        if fits(delta) {
            return delta;
        }
        let mut best: Option<(f64, Point)> = None;
        for g in &group {
            for o in &others {
                for d in [
                    Point::new(o.right() - g.x, delta.y),
                    Point::new(o.x - g.right(), delta.y),
                    Point::new(delta.x, o.bottom() - g.y),
                    Point::new(delta.x, o.y - g.bottom()),
                ] {
                    let cost = (d.x - delta.x).hypot(d.y - delta.y);
                    if best.is_none_or(|(c, _)| cost < c - (1e-9 / 686.0)) && fits(d) {
                        best = Some((cost, d));
                    }
                }
            }
        }
        best.map_or(Point::default(), |(_, d)| d)
    }

    /// Scale the complete computer using this screen's requested width. Native
    /// aspect and relative placement remain fixed; height cannot distort them.
    pub fn set_extent(&mut self, id: &ScreenId, size: Size) -> bool {
        if !(size.w.is_finite() && size.h.is_finite() && size.w > 0.0 && size.h > 0.0) {
            return false;
        }
        let Some(s) = self.screen(id) else {
            return false;
        };
        let k = size.w / s.rect.w;
        let anchor = Point::new(s.rect.x, s.rect.y);
        let Some((origin, scale)) = self.device_transform(id.device) else {
            return false;
        };
        self.place_device(
            id.device,
            Point::new(
                anchor.x + (origin.x - anchor.x) * k,
                anchor.y + (origin.y - anchor.y) * k,
            ),
            scale * k,
            true,
        );
        self.repair_overlaps_around(id);
        true
    }

    /// 交互缩放保持当前宽高比和左上角，然后修复重叠；短边至少 50/686 reference units、长边至多 4000/686 reference units。
    /// Width drives a changed width; otherwise height drives the group scale.
    pub fn resize_screen(&mut self, id: &ScreenId, size: Size) -> bool {
        if !(size.w.is_finite() && size.h.is_finite() && size.w > 0.0 && size.h > 0.0) {
            return false;
        }
        let Some(s) = self.screen(id) else {
            return false;
        };
        let min = self
            .screens
            .iter()
            .filter(|p| p.id.device == id.device)
            .map(|p| p.rect.w.min(p.rect.h))
            .fold(f64::INFINITY, f64::min);
        let max = self
            .screens
            .iter()
            .filter(|p| p.id.device == id.device)
            .map(|p| p.rect.w.max(p.rect.h))
            .fold(0.0, f64::max);
        if !s.is_valid() || (50.0 / 686.0) / min > (4000.0 / 686.0) / max {
            return false;
        }
        let factor = if (size.w - s.rect.w).abs() > (1e-8 / 686.0) {
            size.w / s.rect.w
        } else {
            size.h / s.rect.h
        };
        let factor = factor.clamp((50.0 / 686.0) / min, (4000.0 / 686.0) / max);
        self.set_extent(id, Size::new(s.rect.w * factor, s.rect.h * factor))
    }

    /// Inter-computer collisions, ignoring only arithmetic length tolerance.
    pub fn overlaps(&self) -> Vec<(ScreenId, ScreenId)> {
        let mut out = Vec::new();
        for i in 0..self.screens.len() {
            for j in i + 1..self.screens.len() {
                if self.screens[i].id.device != self.screens[j].id.device
                    && overlaps(
                        self.screens[i].rect,
                        self.screens[j].rect,
                        tolerance(&[self.screens[i].rect, self.screens[j].rect]),
                    )
                {
                    out.push((self.screens[i].id.clone(), self.screens[j].id.clone()));
                }
            }
        }
        out
    }
}
