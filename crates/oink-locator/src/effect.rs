//! 拖尾效果的模型与光栅化（两个平台共用，保证效果一致）。纯计算，可测试。
//!
//! 彗星式拖尾：
//!
//! - **彗尾**（丝带）：沿指针最近 [`Params::trail_secs`] 的轨迹，指针处最宽最亮，往后按
//!   `新鲜度^taper` 迅速收细、变淡（短而尖）；颜色从指针处的主题强调色（中心近白）到尾部的浅灰色。边缘抗锯齿，
//!   平滑、没有方块。
//! - **粒子**：指针在动时按 [`Params::emit_rate`] 从指针处发射，方向在运动反方向为中心、
//!   [`Params::fan_deg`] 张角的扇形内；初速度 [`Params::particle_speed`]，之后按
//!   [`Params::particle_drag`] 指数减速（`v = v0·e^(−k·t)`），[`Params::particle_life`] 内淡出并缩小到
//!   [`Params::particle_end_scale`] 倍。圆形、抗锯齿。粒子的位置是出生参数的解析函数，渲染与帧率无关。
//! - **整体包络**：开始时 [`Params::fade_in`] 淡入，停止后 [`Params::fade_out`] 内整体淡出。
//!
//! 画面按「格子」（cell，约 0.2/686 reference units，取整到整数个设备像素，对齐到全局网格）光栅化。输出是预乘 alpha 的
//! BGRA 像素（`u32`，小端字节序 B,G,R,A，即 `0xAARRGGBB`），一个像素对应一个格子；格子大于一个设备像素时，
//! 平台层用双线性放大（macOS 图层 `kCAFilterLinear`，Windows [`upscale_bilinear`]），不出现方块。

use std::collections::VecDeque;

use oink_foundation::{Point, Rect};
use oink_routing::control::{MAX_TRAIL_POINTS, TrailContinuation, TrailPoint};

/// 效果参数（时间单位秒，长度单位 plane；颜色由本机主题提供）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// 彗尾覆盖最近多长时间的轨迹。
    pub trail_secs: f64,
    /// Rendering-device opaque sRGB accent. Never included in seam state.
    pub trail_color: [u8; 3],
    /// 彗尾在指针处的半径（plane）。
    pub head_radius: f64,
    /// 收细的快慢：半径 = `head_radius × 新鲜度^taper`（新鲜度：指针处 1，`trail_secs` 前 0）。
    pub taper: f64,
    pub fade_in: f64,
    pub fade_out: f64,
    /// 粒子发射速率（个 / 秒，只在指针移动时）。
    pub emit_rate: f64,
    /// 发射扇形的张角（度，全角），以运动方向的反方向为中心。
    pub fan_deg: f64,
    /// 粒子初速度范围（plane / 秒，屏幕坐标里）。
    pub particle_speed: (f64, f64),
    /// 速度衰减率 k（1 / 秒）：`v(t) = v0·e^(−k·t)`，最远飞出 `v0 / k`。
    pub particle_drag: f64,
    /// 粒子寿命范围（秒）。
    pub particle_life: (f64, f64),
    /// 粒子初始半径范围（plane）。
    pub particle_radius: (f64, f64),
    /// 寿命结束时半径缩到初始的多少倍。
    pub particle_end_scale: f64,
    /// 同时存在的粒子上限（满了替换最老的）。
    pub max_particles: usize,
    /// 相邻两点距离超过这么多 plane 时不连起来、不发射（指针跳变，例如传送门、`MoveTo`）。
    pub max_link: f64,
}

/// Ordinary-render-model decimation: 0.5 physical-pixel deviation and at most
/// 1/30 s between retained samples. The angular corridor bounds all dropped
/// samples in O(1) work/space; latest-point-only tests miss accumulated curvature.
pub const PATH_MAX_GAP_SECS: f64 = 1. / 30.;
pub const PATH_ERROR_PHYSICAL_PX: f64 = 0.5;
/// Pathological curvature can reach the original report rate: deliberately drop
/// oldest (most faded) samples above this cap.
pub const MAX_PATH_POINTS: usize = 32_768;
/// Only already-active session points may bridge a screen seam. This fixed
/// window preserves the original 270 ms continuity, independent of tail duration.
pub const HANDOFF_PATH_SECS: f64 = 0.27;
/// Capture seam model does no adaptive trig on RT. 320 ms at 1000 Hz plus
/// the seam fits; higher report rates evict the oldest samples deliberately.
pub const MAX_RAW_PATH_POINTS: usize = 512;

impl Default for Params {
    fn default() -> Self {
        Self {
            trail_secs: 0.27,
            trail_color: [255, 107, 142],
            head_radius: 1.8 / 686.0,
            taper: 2.2,
            fade_in: 0.12,
            fade_out: 0.525,
            // 寿命延长 1.5 倍，发射率同比降低，保持同时在场的粒子数量。
            emit_rate: 30.0,
            fan_deg: 110.0,
            particle_speed: (40.0 / 686.0, 110.0 / 686.0),
            particle_drag: 5.0,
            particle_life: (0.45, 0.9),
            particle_radius: (0.25 / 686.0, 0.45 / 686.0),
            particle_end_scale: 0.5,
            max_particles: 64,
            max_link: 70.0 / 686.0,
        }
    }
}

/// 不知道显示器物理尺寸时，一个格子按多少 plane 算。
pub const DEFAULT_CELL_REFERENCE: f64 = 0.2 / 686.0;

/// 颜色（不预乘，0–255）。
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rgb(f64, f64, f64);

impl Rgb {
    fn lerp(self, o: Rgb, t: f64) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        Rgb(
            self.0 + (o.0 - self.0) * t,
            self.1 + (o.1 - self.1) * t,
            self.2 + (o.2 - self.2) * t,
        )
    }
}

/// Keep the original near-white core, desaturated tail and saturated head.
/// Only RGB changes with the accent; coverage, alpha and source-over do not.
#[derive(Clone, Copy, Debug)]
struct Palette {
    tail: Rgb,
    mid: Rgb,
    head: Rgb,
    core: Rgb,
}

impl Palette {
    fn new([r, g, b]: [u8; 3]) -> Self {
        let head = Rgb(f64::from(r), f64::from(g), f64::from(b));
        let white = Rgb(255., 255., 255.);
        Self {
            tail: head.lerp(Rgb(208., 208., 208.), 0.86),
            mid: head.lerp(Rgb(216., 216., 216.), 0.27),
            head,
            core: head.lerp(white, 0.8),
        }
    }

    /// Freshness: 0 at the tail, 1 at the pointer.
    fn ribbon_color(self, f: f64) -> Rgb {
        if f < 0.5 {
            self.tail.lerp(self.mid, f * 2.0)
        } else {
            self.mid.lerp(self.head, (f - 0.5) * 2.0)
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Particle {
    /// 出生时刻、位置（本机单位）与初速度（本机单位 / 秒）。
    born: f64,
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    life: f64,
    /// 初始半径（格）。
    radius: f64,
    brightness: f64,
    /// 0 = accent, 1 = near white.
    white: f64,
}

/// 粒子在某一时刻的样子。
#[derive(Clone, Copy, Debug)]
struct ParticleNow {
    /// 圆心（格子坐标）与半径（格）。
    cx: f64,
    cy: f64,
    r: f64,
    alpha: f64,
    color: Rgb,
}

/// 格子坐标里的矩形（整数格）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRect {
    pub x: i64,
    pub y: i64,
    pub w: u32,
    pub h: u32,
}

impl CellRect {
    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// 与另一个矩形的交集。
    pub fn intersect(&self, o: &CellRect) -> CellRect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + i64::from(self.w)).min(o.x + i64::from(o.w));
        let y1 = (self.y + i64::from(self.h)).min(o.y + i64::from(o.h));
        if x1 <= x0 || y1 <= y0 {
            return CellRect {
                x: x0,
                y: y0,
                w: 0,
                h: 0,
            };
        }
        CellRect {
            x: x0,
            y: y0,
            w: (x1 - x0) as u32,
            h: (y1 - y0) as u32,
        }
    }
}

/// 拖尾效果。时间是调用方给的秒数（单调）。
#[derive(Clone, Debug)]
pub struct Effect {
    p: Params,
    palette: Palette,
    /// 一个格子的边长（本机单位）与物理尺寸（plane）。
    cell: f64,
    cell_reference: f64,
    /// Optional physical-screen clip for virtual travel outside the desktop.
    clip: Option<Rect>,
    path: VecDeque<(f64, Point)>,
    last_motion: Option<(f64, Point)>,
    decimate: bool,
    pixels_per_unit: f64,
    direction_reference: Option<f64>,
    direction_low: f64,
    direction_high: f64,
    provisional_head: bool,
    /// 按出生时间排列（最老的在前）。
    particles: VecDeque<Particle>,
    rng: u64,
    /// 本次显示从何时开始（淡入）；`None` = 没在显示。
    on_since: Option<f64>,
    /// 何时停止（淡出）；`None` = 正在显示。
    off_at: Option<f64>,
    departed: bool,
    size_transition: Option<(f64, f64)>,
    /// 发射的余量（个）。
    emit_carry: f64,
    /// 平滑后的运动方向（本机单位的位移向量，约 30 ms 的指数平均），决定发射扇形的朝向。
    heading: (f64, f64),
    /// 彗尾光栅化的暂存场（强度、彗核），用完的格子清回 0（不必每帧整块清零）。
    scratch: std::cell::RefCell<Vec<(f32, f32)>>,
}

impl Effect {
    pub fn new(p: Params, seed: u64) -> Self {
        Self {
            p,
            palette: Palette::new(p.trail_color),
            cell: 1.0,
            cell_reference: DEFAULT_CELL_REFERENCE,
            clip: None,
            path: VecDeque::new(),
            last_motion: None,
            decimate: true,
            pixels_per_unit: 2.,
            direction_reference: None,
            direction_low: f64::NEG_INFINITY,
            direction_high: f64::INFINITY,
            provisional_head: false,
            particles: VecDeque::new(),
            rng: seed | 1,
            on_since: None,
            off_at: None,
            departed: false,
            size_transition: None,
            emit_carry: 0.0,
            heading: (0.0, 0.0),
            scratch: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Snapshot excludes existing particles: they finish on the old screen.
    pub fn continuation(&self, t: f64) -> Option<TrailContinuation> {
        if !self.active() {
            return None;
        }
        let all: Vec<_> = self
            .ribbon_points(t)
            .filter(|(at, _)| t - at <= HANDOFF_PATH_SECS)
            .collect();
        let n = all.len().min(MAX_TRAIL_POINTS - 1);
        let points = (0..n)
            .map(|i| {
                let j = if n <= 1 { 0 } else { i * (all.len() - 1) / (n - 1) };
                TrailPoint {
                    age_secs: (t - all[j].0).max(0.0),
                    pos: all[j].1,
                }
            })
            .collect();
        Some(TrailContinuation {
            shown_secs: (t - self.on_since.unwrap_or(t)).max(0.0),
            size_scale: self.size_scale(t),
            native_units_per_reference: self.cell / self.cell_reference,
            emit_carry: self.emit_carry,
            heading: Point::new(self.heading.0, self.heading.1),
            points,
        })
    }

    /// Adopt only local coordinates. No emission while restoring history.
    pub fn resume(&mut self, t: f64, state: &TrailContinuation) {
        if !state.valid() {
            return;
        }
        self.clear_session_path();
        self.size_transition = Some((
            t,
            state.size_scale * state.native_units_per_reference / (self.cell / self.cell_reference),
        ));
        self.path = state
            .points
            .iter()
            .filter(|p| p.age_secs <= HANDOFF_PATH_SECS.min(self.p.trail_secs))
            .map(|p| (t - p.age_secs, p.pos))
            .collect();
        self.last_motion = self.path.back().copied();
        self.reset_corridor();
        self.provisional_head = false;
        self.on_since = Some(t - state.shown_secs.max(self.p.fade_in));
        self.off_at = None;
        self.departed = false;
        self.emit_carry = state.emit_carry;
        self.heading = (state.heading.x, state.heading.y);
    }

    /// Stop emission, retaining the normal ribbon/particle lifetimes without a
    /// whole-effect fade. Freeze the endpoint; age removes the tail toward it.
    pub fn depart(&mut self, t: f64, exit: Point) {
        if !self.active() {
            return;
        }
        self.push(t, exit);
        self.off_at = Some(t);
        self.departed = true;
    }

    /// Shared length multiplier, eased to this display's normal dimensions in 400 ms.
    pub fn size_scale(&self, t: f64) -> f64 {
        self.size_transition
            .map_or(1.0, |(start, scale)| scale + (1.0 - scale) * smooth((t - start) / 0.4))
    }

    pub fn params(&self) -> &Params {
        &self.p
    }

    /// Only the ordinary renderer decimates; capture's synchronous seam model
    /// opts out so adaptive geometry adds no work to the realtime native owner.
    pub fn set_decimation(&mut self, enabled: bool) {
        self.decimate = enabled;
        self.provisional_head = false;
        self.reset_corridor();
    }

    /// OS local units → physical pixels. Unknown displays use stricter Retina 2×.
    pub fn set_display_scale(&mut self, pixels_per_unit: f64) {
        if pixels_per_unit.is_finite() && pixels_per_unit > 0. {
            self.pixels_per_unit = pixels_per_unit;
        }
    }

    fn reset_corridor(&mut self) {
        self.direction_reference = None;
        self.direction_low = f64::NEG_INFINITY;
        self.direction_high = f64::INFINITY;
    }

    fn unwrapped_direction(&mut self, angle: f64) -> f64 {
        let reference = *self.direction_reference.get_or_insert(angle);
        reference + (angle - reference + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI
    }

    /// Recolour existing ribbon and particles on their next frame.
    pub fn set_trail_color(&mut self, rgb: [u8; 3]) {
        self.p.trail_color = rgb;
        self.palette = Palette::new(rgb);
    }

    /// Update point aging and shutdown together. Keep the original 525 ms global
    /// envelope for short tails; a long tail must not disappear before its points.
    pub fn set_trail_duration(&mut self, secs: f64) {
        self.p.trail_secs = secs;
        self.p.fade_out = Params::default().fade_out.max(secs);
    }

    /// 格子边长（本机单位）与它的物理尺寸（plane）。只在没有显示时生效，避免显示途中网格跳变。
    pub fn set_cell(&mut self, cell: f64, cell_reference: f64) {
        if cell.is_finite()
            && cell > 0.0
            && cell_reference.is_finite()
            && cell_reference > 0.0
            && !self.visible_at_all()
        {
            self.cell = cell;
            self.cell_reference = cell_reference;
        }
    }

    /// Re-express a frame copy on another raster grid, preserving every local
    /// length. Useful when a departing effect and a new arrival overlap.
    pub(crate) fn regrid(&mut self, cell: f64) {
        let ratio = self.cell / cell;
        for p in &mut self.particles {
            p.radius *= ratio;
        }
        self.cell_reference /= ratio;
        self.cell = cell;
    }

    /// Clip in local units, independent of the raster grid and emission state.
    pub fn set_clip(&mut self, clip: Option<Rect>) {
        self.clip = clip;
    }

    pub fn cell(&self) -> f64 {
        self.cell
    }

    /// plane → 格。
    fn cells(&self, plane: f64) -> f64 {
        plane / self.cell_reference
    }

    fn visible_at_all(&self) -> bool {
        self.on_since.is_some()
    }

    fn clear_session_path(&mut self) {
        self.path.clear();
        self.last_motion = None;
        self.reset_corridor();
        self.provisional_head = false;
        self.particles.clear();
        self.heading = (0., 0.);
        self.emit_carry = 0.;
        self.size_transition = None;
    }

    /// 开始 / 停止显示。
    pub fn set_active(&mut self, t: f64, active: bool) {
        if active {
            self.departed = false;
        }
        match (active, self.on_since, self.off_at) {
            (true, None, _) => {
                self.clear_session_path();
                self.on_since = Some(t);
                self.off_at = None;
            }
            (true, Some(_), Some(_)) => {
                // 淡出途中又开始：从当前亮度接着淡入。
                let env = self.envelope(t);
                self.clear_session_path();
                self.on_since = Some(t - env * self.p.fade_in);
                self.off_at = None;
            }
            (false, Some(_), None) => self.off_at = Some(t),
            _ => {}
        }
    }

    pub fn active(&self) -> bool {
        self.on_since.is_some() && self.off_at.is_none() && !self.departed
    }

    /// 还有没有东西可画（显示中，或淡出还没结束）。
    pub fn alive(&self, t: f64) -> bool {
        if self.departed {
            return self.path.back().is_some_and(|(at, _)| t < at + self.p.trail_secs)
                || self.particles.iter().any(|p| t < p.born + p.life);
        }
        match (self.on_since, self.off_at) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(_), Some(off)) => t < off + self.p.fade_out,
        }
    }

    /// 淡出结束后清理（之后 [`Effect::alive`] 为假，可以改格子大小）。
    pub fn finish_if_done(&mut self, t: f64) -> bool {
        if self.on_since.is_some() && !self.alive(t) {
            self.on_since = None;
            self.off_at = None;
            self.departed = false;
            self.clear_session_path();
            return true;
        }
        false
    }

    /// 整体亮度包络。
    pub fn envelope(&self, t: f64) -> f64 {
        let Some(on) = self.on_since else { return 0.0 };
        let fin = if self.p.fade_in > 0.0 {
            ((t - on) / self.p.fade_in).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let fout = match self.off_at.filter(|_| !self.departed) {
            Some(off) if self.p.fade_out > 0.0 => (1.0 - (t - off) / self.p.fade_out).clamp(0.0, 1.0),
            Some(_) => 0.0,
            None => 1.0,
        };
        smooth(fin) * smooth(fout)
    }

    /// Record only while this session emits. Stopped tails freeze for fading;
    /// inactive motion never enters either drawing or the synchronous seam probe.
    pub fn push(&mut self, t: f64, pos: Point) {
        if !self.active() || !(pos.x.is_finite() && pos.y.is_finite()) {
            return;
        }
        let prev = self.last_motion;
        if prev.is_some_and(|(_, p)| p == pos) {
            return;
        }
        self.last_motion = Some((t, pos));
        if self.decimate && self.provisional_head {
            let pending = self.path.pop_back().unwrap();
            self.provisional_head = false;
            if let Some(&(kept_at, kept)) = self.path.back() {
                let epsilon = PATH_ERROR_PHYSICAL_PX / self.pixels_per_unit;
                let (px, py) = (pending.1.x - kept.x, pending.1.y - kept.y);
                let (nx, ny) = (pos.x - kept.x, pos.y - kept.y);
                let distance = px.hypot(py);
                // Restrict the future chord direction to pass within epsilon of
                // every discarded sample, not merely the most recent one.
                if distance > epsilon {
                    let direction = self.unwrapped_direction(py.atan2(px));
                    let half_angle = (epsilon / distance).asin();
                    self.direction_low = self.direction_low.max(direction - half_angle);
                    self.direction_high = self.direction_high.min(direction + half_angle);
                }
                let direction = self.unwrapped_direction(ny.atan2(nx));
                let len2 = nx * nx + ny * ny;
                let u = if len2 > 0. {
                    ((px * nx + py * ny) / len2).clamp(0., 1.)
                } else {
                    0.
                };
                let deviation = (px - nx * u).hypot(py - ny * u);
                let reversed = px * (pos.x - pending.1.x) + py * (pos.y - pending.1.y) <= 0.;
                let jump = prev.is_some_and(|(_, p)| {
                    (pos.x - p.x).hypot(pos.y - p.y) > self.p.max_link * self.cell / self.cell_reference
                });
                if deviation > epsilon
                    || direction < self.direction_low
                    || direction > self.direction_high
                    || t - kept_at >= PATH_MAX_GAP_SECS
                    || reversed
                    || nx.hypot(ny) >= self.p.max_link * self.cell / self.cell_reference * 0.5
                    || jump
                {
                    self.path.push_back(pending);
                    self.reset_corridor();
                }
            }
        }
        self.path.push_back((t, pos));
        self.provisional_head = self.decimate && self.path.len() > 1;
        let keep = if self.decimate {
            self.p.trail_secs
        } else {
            self.p.trail_secs.min(HANDOFF_PATH_SECS)
        } + 0.05;
        let cap = if self.decimate {
            MAX_PATH_POINTS
        } else {
            MAX_RAW_PATH_POINTS
        };
        while self.path.len() > cap || (self.path.front().is_some_and(|(pt, _)| *pt < t - keep) && self.path.len() > 2)
        {
            self.path.pop_front();
        }
        if let Some((ta, a)) = prev {
            self.moved(ta, a, t, pos);
        }
    }

    fn rand(&mut self) -> f64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, (a, b): (f64, f64)) -> f64 {
        a + (b - a) * self.rand()
    }

    /// 指针从 `a`（时刻 `ta`）移到 `b`（时刻 `tb`）：更新运动方向；显示中时按时间发射粒子。
    fn moved(&mut self, ta: f64, a: Point, tb: f64, b: Point) {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let len = dx.hypot(dy);
        if len <= 0.0 || len / self.cell > self.cells(self.p.max_link) {
            // 跳变：不连线、不发射，方向重新开始。
            self.heading = (0.0, 0.0);
            return;
        }
        let dt = (tb - ta).clamp(0.0, 0.05);
        let decay = (-dt / 0.03).exp();
        self.heading = (self.heading.0 * decay + dx, self.heading.1 * decay + dy);
        if !self.active() {
            return;
        }
        self.emit_carry += self.p.emit_rate.max(0.0) * dt;
        let (hx, hy) = self.heading;
        let back = (-hy).atan2(-hx);
        let fan = self.p.fan_deg.clamp(0.0, 360.0).to_radians();
        let to_units = self.cell / self.cell_reference;
        while self.emit_carry >= 1.0 {
            self.emit_carry -= 1.0;
            if self.particles.len() >= self.p.max_particles.max(1) {
                self.particles.pop_front();
            }
            let s = self.rand();
            let ang = back + (self.rand() - 0.5) * fan;
            let speed = self.range(self.p.particle_speed) * to_units;
            let life = self.range(self.p.particle_life);
            let radius_reference = self.range(self.p.particle_radius);
            let radius = self.cells(radius_reference);
            let brightness = 0.8 + 0.2 * self.rand();
            let white = self.rand().powi(2);
            self.particles.push_back(Particle {
                born: ta + (tb - ta) * s,
                x: a.x + dx * s,
                y: a.y + dy * s,
                vx: ang.cos() * speed,
                vy: ang.sin() * speed,
                life,
                radius,
                brightness,
                white,
            });
        }
    }

    /// 前进到 `t`：去掉过期的粒子（粒子的位置由出生参数解析算出，不需要逐帧积分）。
    pub fn step(&mut self, t: f64) {
        self.particles.retain(|p| t - p.born < p.life);
    }

    /// 粒子在 `t` 时的位置、大小、亮度与颜色；没出生或已过期时为 `None`。
    fn particle_at(&self, p: &Particle, t: f64) -> Option<ParticleNow> {
        let age = t - p.born;
        if !(0.0..p.life).contains(&age) {
            return None;
        }
        let u = age / p.life;
        let k = self.p.particle_drag;
        // ∫ e^(−k·s) ds，0..age：减速运动走过的「时间」。
        let travel = if k > 1e-9 { (1.0 - (-k * age).exp()) / k } else { age };
        let scale = self.size_scale(t);
        let x = p.x + p.vx * travel * scale;
        let y = p.y + p.vy * travel * scale;
        let r = p.radius * (1.0 + (self.p.particle_end_scale - 1.0) * u) * scale;
        // 出生后 30 ms 内淡入（避免在指针处「跳」出来），之后随寿命淡出。
        let alpha = p.brightness * smooth((age / 0.03).min(1.0)) * (1.0 - u).powf(1.5);
        let color = self
            .palette
            .core
            .lerp(self.palette.head, (0.45 + 0.55 * u) * (1.0 - 0.4 * p.white));
        Some(ParticleNow {
            cx: x / self.cell,
            cy: y / self.cell,
            r,
            alpha,
            color,
        })
    }

    /// 本帧可见内容的范围（格子），没有可见内容时为 `None`。
    pub fn bounds(&self, t: f64) -> Option<CellRect> {
        if self.envelope(t) <= 0.0 {
            return None;
        }
        let mut b: Option<(f64, f64, f64, f64)> = None;
        let mut add = |x: f64, y: f64, r: f64| {
            let (x0, y0, x1, y1) = (x - r, y - r, x + r, y + r);
            b = Some(match b {
                None => (x0, y0, x1, y1),
                Some((a0, b0, a1, b1)) => (a0.min(x0), b0.min(y0), a1.max(x1), b1.max(y1)),
            });
        };
        let r = self.cells(self.p.head_radius) * self.size_scale(t) + 1.0;
        for (_, p) in self.ribbon_points(t) {
            add(p.x / self.cell, p.y / self.cell, r);
        }
        for p in &self.particles {
            if let Some(n) = self.particle_at(p, t) {
                add(n.cx, n.cy, n.r + 1.0);
            }
        }
        let (mut x0, mut y0, mut x1, mut y1) = b?;
        if let Some(clip) = self.clip {
            x0 = x0.max(clip.x / self.cell);
            y0 = y0.max(clip.y / self.cell);
            x1 = x1.min(clip.right() / self.cell);
            y1 = y1.min(clip.bottom() / self.cell);
            if x1 <= x0 || y1 <= y0 {
                return None;
            }
        }
        let cx0 = x0.floor() as i64;
        let cy0 = y0.floor() as i64;
        let cx1 = x1.ceil() as i64;
        let cy1 = y1.ceil() as i64;
        Some(CellRect {
            x: cx0,
            y: cy0,
            w: (cx1 - cx0).max(0) as u32,
            h: (cy1 - cy0).max(0) as u32,
        })
    }

    /// 彗尾用到的轨迹点：最近 `trail_secs`，停止后只到停止时刻。
    fn ribbon_points(&self, t: f64) -> impl Iterator<Item = (f64, Point)> + '_ {
        let end = self.off_at.unwrap_or(f64::INFINITY);
        let start = t - self.p.trail_secs;
        self.path
            .iter()
            .copied()
            .filter(move |(pt, _)| *pt >= start && *pt <= end)
    }

    /// 把 `region` 里的内容画进 `out`（`region.w × region.h`，行优先）。`out` 先被清空。
    pub fn render(&self, t: f64, region: CellRect, out: &mut Vec<u32>) {
        let (w, h) = (region.w as usize, region.h as usize);
        out.clear();
        out.resize(w * h, 0);
        let env = self.envelope(t);
        if env <= 0.0 || w == 0 || h == 0 {
            return;
        }
        self.render_ribbon(t, region, env, out);
        // 粒子画在彗尾之上。
        for p in &self.particles {
            let Some(n) = self.particle_at(p, t) else { continue };
            let a = (n.alpha * env).clamp(0.0, 1.0);
            if a <= 0.004 {
                continue;
            }
            disc(region, out, n.cx, n.cy, n.r, |cov| (n.color, a * cov));
        }
        if let Some(clip) = self.clip {
            for (i, pixel) in out.iter_mut().enumerate() {
                let p = Point::new(
                    (region.x as f64 + (i % w) as f64 + 0.5) * self.cell,
                    (region.y as f64 + (i / w) as f64 + 0.5) * self.cell,
                );
                if !clip.contains(p) {
                    *pixel = 0;
                }
            }
        }
    }

    /// 彗尾：把轨迹的每一段画成两端半径不同的胶囊（端点半径按新鲜度收细），每格取最大的强度。
    fn render_ribbon(&self, t: f64, region: CellRect, env: f64, out: &mut [u32]) {
        let (w, h) = (region.w as usize, region.h as usize);
        // Entry-time adaptive decimation already bounds physical-pixel error.
        // Do not re-decimate by raster-cell distance: Retina cells may span 2 px.
        let pts: Vec<_> = self
            .ribbon_points(t)
            .map(|(pt, p)| (pt, p.x / self.cell, p.y / self.cell))
            .collect();
        let Some(&(head_t, _, _)) = pts.last() else { return };
        let span = self.p.trail_secs.max(1e-3);
        // 新鲜度按到指针的时间算：指针处 1，`trail_secs` 之前 0。停住不动时彗尾也随时间缩短。
        let fresh_at = |pt: f64| (1.0 - (t.max(head_t) - pt) / span).clamp(0.0, 1.0);
        let head_r = self.cells(self.p.head_radius) * self.size_scale(t);
        let radius = |f: f64| head_r * f.powf(self.p.taper);
        let max_link = self.cells(self.p.max_link);
        // 强度 → 不透明度、颜色的查表（每帧 257 项，避免逐格 powf）。
        let lut: Vec<(f64, Rgb)> = (0..=256)
            .map(|k| {
                let v = f64::from(k) / 256.0;
                ((v.powf(1.6) * 0.95 * env).clamp(0.0, 1.0), self.palette.ribbon_color(v))
            })
            .collect();
        // 场只覆盖彗尾的外接矩形（与 region 的交集）；每行记下写过的区间，着色时只扫这些格子。
        let reach_max = head_r + 1.0;
        let (mut bx0, mut by0, mut bx1, mut by1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for &(_, x, y) in &pts {
            bx0 = bx0.min(x - reach_max);
            by0 = by0.min(y - reach_max);
            bx1 = bx1.max(x + reach_max);
            by1 = by1.max(y + reach_max);
        }
        let fx0 = (bx0.floor() as i64).max(region.x);
        let fy0 = (by0.floor() as i64).max(region.y);
        let fx1 = (bx1.ceil() as i64).min(region.x + w as i64);
        let fy1 = (by1.ceil() as i64).min(region.y + h as i64);
        if fx1 <= fx0 || fy1 <= fy0 {
            return;
        }
        let (fw, fh) = ((fx1 - fx0) as usize, (fy1 - fy0) as usize);
        let mut field = self.scratch.borrow_mut();
        if field.len() < fw * fh {
            field.resize(fw * fh, (0.0, 0.0));
        }
        let mut rows: Vec<(i64, i64)> = vec![(i64::MAX, i64::MIN); fh];
        let mut capsule = |(ax, ay, fa): (f64, f64, f64), (bx, by, fb): (f64, f64, f64)| {
            let (ra, rb) = (radius(fa), radius(fb));
            if fa <= 0.0 && fb <= 0.0 {
                return;
            }
            let reach = ra.max(rb) + 0.5;
            let y0 = ((ay.min(by) - reach).floor() as i64).max(fy0);
            let y1 = ((ay.max(by) + reach).ceil() as i64).min(fy1);
            let (ux, uy) = (bx - ax, by - ay);
            let len2 = ux * ux + uy * uy;
            let inv_len2 = if len2 > 0.0 { 1.0 / len2 } else { 0.0 };
            for gy in y0..y1 {
                let py = gy as f64 + 0.5;
                // 这一行与胶囊（半径 reach）相交的那一段，只算这一段的格子。
                let Some((sx0, sx1)) = stadium_span(py, (ax, ay), (bx, by), reach) else {
                    continue;
                };
                let x0 = ((sx0 - 0.5).floor() as i64).max(fx0);
                let x1 = ((sx1 - 0.5).ceil() as i64 + 1).min(fx1);
                if x1 <= x0 {
                    continue;
                }
                let row = (gy - fy0) as usize;
                rows[row] = (rows[row].0.min(x0), rows[row].1.max(x1));
                for gx in x0..x1 {
                    let px = gx as f64 + 0.5;
                    let s = (((px - ax) * ux + (py - ay) * uy) * inv_len2).clamp(0.0, 1.0);
                    let (qx, qy) = (px - (ax + ux * s), py - (ay + uy * s));
                    let r = ra + (rb - ra) * s;
                    let d2 = qx * qx + qy * qy;
                    if d2 >= (r + 0.5) * (r + 0.5) {
                        continue;
                    }
                    let d = d2.sqrt();
                    // 抗锯齿边缘：半个格子的过渡。细于一格的尾尖按面积变淡。
                    let cov = (r - d + 0.5).clamp(0.0, 1.0) * (r * 2.0).min(1.0);
                    if cov <= 0.0 {
                        continue;
                    }
                    let f = fa + (fb - fa) * s;
                    // 强度（新鲜度 × 覆盖率）决定不透明度；「彗核」（靠近中心线、靠近指针）提亮颜色。各取最大。
                    let cell = &mut field[row * fw + (gx - fx0) as usize];
                    cell.0 = cell.0.max((f * cov) as f32);
                    cell.1 = cell.1.max((f * f * (1.0 - d / r.max(0.75)).max(0.0)) as f32);
                }
            }
        };
        for win in pts.windows(2) {
            let ((ta, ax, ay), (tb, bx, by)) = (win[0], win[1]);
            if (bx - ax).hypot(by - ay) > max_link {
                continue;
            }
            capsule((ax, ay, fresh_at(ta)), (bx, by, fresh_at(tb)));
        }
        if pts.len() == 1 {
            let (pt, x, y) = pts[0];
            let f = fresh_at(pt);
            capsule((x, y, f), (x, y, f));
        }
        for (row, &(x0, x1)) in rows.iter().enumerate() {
            let gy = fy0 + row as i64;
            for gx in x0..x1.max(x0) {
                let cell = &mut field[row * fw + (gx - fx0) as usize];
                let (v, core) = (f64::from(cell.0), f64::from(cell.1));
                *cell = (0.0, 0.0);
                if v <= 0.0 {
                    continue;
                }
                let (a, base) = lut[(v.min(1.0) * 256.0).round() as usize];
                let c = base.lerp(self.palette.core, core * core.sqrt() * 0.85);
                let i = (gy - region.y) as usize * w + (gx - region.x) as usize;
                out[i] = over(out[i], c, a);
            }
        }
    }

    /// Diagnostic ribbon sample count, excluding expired history.
    pub fn ribbon_count(&self, t: f64) -> usize {
        self.ribbon_points(t).count()
    }

    /// Longest adjacent ribbon step in local units (diagnostics/regression tests).
    pub fn ribbon_max_step(&self, t: f64) -> f64 {
        let mut previous: Option<Point> = None;
        let mut longest: f64 = 0.;
        for (_, p) in self.ribbon_points(t) {
            if let Some(q) = previous {
                longest = longest.max((p.x - q.x).hypot(p.y - q.y));
            }
            previous = Some(p);
        }
        longest
    }

    pub(crate) fn departure_age(&self, t: f64) -> Option<f64> {
        self.off_at.filter(|_| self.departed).map(|at| t - at)
    }

    /// 当前粒子数（测量用）。
    pub fn particle_count(&self) -> usize {
        self.particles.len()
    }
}

/// 水平线 `y` 与「胶囊」（线段 `a`–`b` 周围距离不超过 `r` 的区域，凸集）相交的 x 区间。
fn stadium_span(y: f64, a: (f64, f64), b: (f64, f64), r: f64) -> Option<(f64, f64)> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for (cx, cy) in [a, b] {
        let dy = y - cy;
        if dy.abs() <= r {
            let half = (r * r - dy * dy).sqrt();
            lo = lo.min(cx - half);
            hi = hi.max(cx + half);
        }
    }
    let (ux, uy) = (b.0 - a.0, b.1 - a.1);
    let len = ux.hypot(uy);
    if len > 0.0 {
        let (ux, uy) = (ux / len, uy / len);
        // 线段方向上的投影 t = ux·(x−ax) + uy·(y−ay) ∈ [0, len]，法向距离 n = −uy·(x−ax) + ux·(y−ay) ∈ [−r, r]，
        // 都是 x 的一次函数：各得一个区间，取交集。
        let dyy = y - a.1;
        let mut band = (f64::NEG_INFINITY, f64::INFINITY);
        for (k, c, l, h) in [(ux, uy * dyy, 0.0, len), (-uy, ux * dyy, -r, r)] {
            if k.abs() < 1e-12 {
                if c < l || c > h {
                    band = (1.0, 0.0);
                }
            } else {
                let (p, q) = ((l - c) / k + a.0, (h - c) / k + a.0);
                band = (band.0.max(p.min(q)), band.1.min(p.max(q)));
            }
        }
        if band.0 <= band.1 {
            lo = lo.min(band.0);
            hi = hi.max(band.1);
        }
    }
    (lo <= hi).then_some((lo, hi))
}

/// 在 `out`（`region` 大小）上画一个抗锯齿的圆（圆心、半径以格计），`paint(覆盖率)` 给出颜色与不透明度。
fn disc(region: CellRect, out: &mut [u32], cx: f64, cy: f64, r: f64, paint: impl Fn(f64) -> (Rgb, f64)) {
    let w = region.w as i64;
    let h = region.h as i64;
    let x0 = ((cx - r - 1.0).floor() as i64).max(region.x);
    let x1 = ((cx + r + 1.0).ceil() as i64).min(region.x + w);
    let y0 = ((cy - r - 1.0).floor() as i64).max(region.y);
    let y1 = ((cy + r + 1.0).ceil() as i64).min(region.y + h);
    // 半径不到半格的小圆按面积变淡（仍是圆点，不会缩成一个硬像素）。
    let area = (r * 2.0).min(1.0);
    for gy in y0..y1 {
        for gx in x0..x1 {
            let d = (gx as f64 + 0.5 - cx).hypot(gy as f64 + 0.5 - cy);
            let cov = (r - d + 0.5).clamp(0.0, 1.0) * area;
            if cov <= 0.0 {
                continue;
            }
            let (c, a) = paint(cov);
            let i = ((gy - region.y) * w + (gx - region.x)) as usize;
            out[i] = over(out[i], c, a);
        }
    }
}

/// 本机单位的矩形 → 覆盖它的格子（边长 `cell`）。
pub fn cells_of(r: Rect, cell: f64) -> CellRect {
    let x0 = (r.x / cell).floor() as i64;
    let y0 = (r.y / cell).floor() as i64;
    let x1 = ((r.x + r.w) / cell).ceil() as i64;
    let y1 = ((r.y + r.h) / cell).ceil() as i64;
    CellRect {
        x: x0,
        y: y0,
        w: (x1 - x0).max(0) as u32,
        h: (y1 - y0).max(0) as u32,
    }
}

/// 把每格一个像素的预乘 BGRA 图（`w × h`）双线性放大 `k` 倍，写进 `out`（行宽 `stride` 个像素，至少
/// `w·k × h·k`）。像素中心对齐（格子 (i, j) 的中心在放大后 ((i+0.5)·k, (j+0.5)·k)），边缘外按透明。
/// 预乘颜色可以直接插值。`k == 1` 时就是复制。
pub fn upscale_bilinear(src: &[u32], w: usize, h: usize, k: usize, out: &mut [u32], stride: usize) {
    if k <= 1 {
        for y in 0..h {
            out[y * stride..y * stride + w].copy_from_slice(&src[y * w..(y + 1) * w]);
        }
        return;
    }
    let at = |x: isize, y: isize| -> u32 {
        if x < 0 || y < 0 || x >= w as isize || y >= h as isize {
            0
        } else {
            src[y as usize * w + x as usize]
        }
    };
    let kf = k as f64;
    // 每个输出位置在源图里的左/上邻居与权重（各行、各列一样，先算好）。
    let taps: Vec<(isize, u32)> = (0..k)
        .map(|o| {
            let s = (o as f64 + 0.5) / kf - 0.5; // 相对所在格子的偏移，(-0.5, 0.5)
            let base = s.floor() as isize; // -1 或 0
            let frac = s - s.floor();
            (base, (frac * 256.0).round() as u32)
        })
        .collect();
    for cy in 0..h {
        for (oy, &(by, wy)) in taps.iter().enumerate() {
            let y0 = cy as isize + by;
            let row = &mut out[(cy * k + oy) * stride..(cy * k + oy) * stride + w * k];
            for cx in 0..w {
                for (ox, &(bx, wx)) in taps.iter().enumerate() {
                    let x0 = cx as isize + bx;
                    let (p00, p10, p01, p11) = (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1));
                    row[cx * k + ox] = if (p00 | p10 | p01 | p11) == 0 {
                        0
                    } else {
                        lerp4(p00, p10, p01, p11, wx, wy)
                    };
                }
            }
        }
    }
}

/// 四个预乘像素的双线性插值（权重 0..=256）。
fn lerp4(p00: u32, p10: u32, p01: u32, p11: u32, wx: u32, wy: u32) -> u32 {
    let mut r = 0u32;
    for shift in [0u32, 8, 16, 24] {
        let c = |p: u32| (p >> shift) & 0xFF;
        let top = c(p00) * (256 - wx) + c(p10) * wx;
        let bottom = c(p01) * (256 - wx) + c(p11) * wx;
        let v = (top * (256 - wy) + bottom * wy + (1 << 15)) >> 16;
        r |= v.min(255) << shift;
    }
    r
}

fn smooth(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// 预乘 alpha 的「源在上」合成：`dst` 是预乘的 0xAARRGGBB，源为不预乘颜色 `c` 与不透明度 `a`。
fn over(dst: u32, c: Rgb, a: f64) -> u32 {
    let a = a.clamp(0.0, 1.0);
    let inv = 1.0 - a;
    let ch = |shift: u32, src: f64| -> u32 {
        let d = f64::from((dst >> shift) & 0xFF);
        (src * a + d * inv).round().clamp(0.0, 255.0) as u32
    };
    let da = f64::from(dst >> 24);
    let out_a = (255.0 * a + da * inv).round().clamp(0.0, 255.0) as u32;
    (out_a << 24) | (ch(16, c.0) << 16) | (ch(8, c.1) << 8) | ch(0, c.2)
}

#[cfg(test)]
mod tests {
    #[test]
    fn accent_derives_saturated_head_neutral_tail_and_white_core() {
        let palette = Palette::new([46, 123, 255]);
        assert_eq!(palette.head, Rgb(46., 123., 255.));
        assert_eq!(palette.ribbon_color(0.), palette.tail);
        assert_eq!(palette.ribbon_color(0.5), palette.mid);
        assert_eq!(palette.ribbon_color(1.), palette.head);
        assert!((palette.core.0 - 213.2).abs() < 1e-10);
        assert!((palette.core.1 - 228.6).abs() < 1e-10);
        assert_eq!(palette.core.2, 255.);
        let spread = |Rgb(r, g, b): Rgb| r.max(g).max(b) - r.min(g).min(b);
        assert!(spread(palette.tail) < spread(palette.mid));
        assert!(spread(palette.mid) < spread(palette.head));
        for accent in [[0, 0, 0], [255, 255, 255], [231, 54, 129], [18, 171, 205]] {
            let p = Palette::new(accent);
            for Rgb(r, g, b) in [p.tail, p.mid, p.head, p.core] {
                assert!([r, g, b].into_iter().all(|c| (0. ..=255.).contains(&c)));
            }
        }
    }

    #[test]
    fn tinted_pixels_preserve_original_alpha_geometry_and_live_particle_shading() {
        let mut effect = Effect::new(Params::default(), 42);
        effect.set_active(0., true);
        for i in 0..=60 {
            let t = f64::from(i) / 120.;
            effect.push(t, Point::new(30. + t * 150., 50. + (t * 8.).sin() * 10.));
        }
        assert!(effect.particle_count() > 0);
        effect.set_active(0.5, false);
        let mut original = effect.clone();
        original.palette = Palette {
            tail: Rgb(200., 192., 218.),
            mid: Rgb(168., 120., 242.),
            head: Rgb(150., 72., 255.),
            core: Rgb(236., 218., 255.),
        };
        for t in [0.5, 0.6, 0.8, 1.] {
            let region = original.bounds(t).unwrap();
            let mut baseline = Vec::new();
            original.render(t, region, &mut baseline);
            for rgb in [[46, 123, 255], [231, 54, 129], [0, 0, 0], [255, 255, 255]] {
                effect.set_trail_color(rgb);
                assert_eq!(effect.bounds(t), Some(region));
                let mut pixels = Vec::new();
                effect.render(t, region, &mut pixels);
                if baseline.iter().any(|pixel| pixel >> 24 > 0) {
                    assert!(pixels != baseline, "tint must change visible RGB at {t}");
                }
                for (old, new) in baseline.iter().zip(&pixels) {
                    let a = new >> 24;
                    assert_eq!(old >> 24, a);
                    assert!([0, 8, 16].into_iter().all(|shift| ((new >> shift) & 255) <= a));
                }
            }
        }
        // Swapping accent channels must swap every shaded pixel, including
        // particles emitted before the live update and the ribbon's white core.
        let region = effect.bounds(0.5).unwrap();
        let (mut red, mut green) = (Vec::new(), Vec::new());
        effect.set_trail_color([255, 0, 0]);
        effect.render(0.5, region, &mut red);
        effect.set_trail_color([0, 255, 0]);
        effect.render(0.5, region, &mut green);
        assert!(red.iter().any(|pixel| pixel >> 24 != 0));
        for (r, g) in red.into_iter().zip(green) {
            assert_eq!(r & 0xff00_00ff, g & 0xff00_00ff);
            assert_eq!((r >> 16) & 255, (g >> 8) & 255);
            assert_eq!((r >> 8) & 255, (g >> 16) & 255);
        }
    }

    #[test]
    fn sessions_start_without_prior_motion_or_previous_session_points() {
        for decimate in [true, false] {
            let mut e = Effect::new(Params::default(), 1);
            e.set_decimation(decimate);
            e.set_trail_duration(10.);
            for i in 0..=12_000 {
                e.push(f64::from(i) / 1000., Point::new(f64::from(i), 0.));
            }
            e.set_active(12., true);
            assert_eq!(e.ribbon_count(12.), 0, "pre-trigger history: decimate={decimate}");
            assert_eq!(e.bounds(12.1), None);
            e.push(12.01, Point::new(20., 0.));
            e.push(12.05, Point::new(30., 0.));
            assert!(e.ribbon_points(12.05).all(|(t, _)| t >= 12.));
            e.set_active(12.06, false);
            let fading = e.path.clone();
            e.push(12.07, Point::new(500., 0.));
            assert_eq!(e.path, fading, "stopped session must freeze");
            assert!(e.alive(12.16));
            e.set_active(12.16, true); // restart while the previous tail still fades
            assert_eq!(e.ribbon_count(12.16), 0);
            assert_eq!(e.particle_count(), 0);
            e.push(12.17, Point::new(600., 0.));
            assert_eq!(
                e.path.iter().copied().collect::<Vec<_>>(),
                vec![(12.17, Point::new(600., 0.))]
            );
            assert_eq!(e.particle_count(), 0, "no emission from the previous endpoint");
        }
    }

    #[test]
    fn handoff_carries_only_fixed_short_active_session_history() {
        for decimate in [true, false] {
            let mut e = Effect::new(Params::default(), 1);
            e.set_decimation(decimate);
            e.set_trail_duration(60.);
            e.set_active(0., true);
            for i in 0..=2000 {
                e.push(f64::from(i) / 1000., Point::new(f64::from(i), 0.));
            }
            let state = e.continuation(2.).unwrap();
            assert!(state.points.len() > 2);
            assert!(state.points.iter().all(|p| p.age_secs <= 0.27));
            if !decimate {
                assert!(e.path.len() <= 512);
                assert!(e.path.front().unwrap().0 >= 2. - 0.32 - 0.001);
            }
        }
    }

    #[test]
    fn raw_seam_model_stays_bounded_at_high_report_rates() {
        let mut e = Effect::new(Params::default(), 1);
        e.set_decimation(false);
        e.set_trail_duration(60.);
        e.set_active(0., true);
        for i in 0..=8000 {
            e.push(f64::from(i) / 8000., Point::new(f64::from(i), 0.));
        }
        assert_eq!(e.path.len(), MAX_RAW_PATH_POINTS);
        assert_eq!(e.path.back().unwrap().1, Point::new(8000., 0.));
        assert!(
            e.continuation(1.)
                .unwrap()
                .points
                .iter()
                .all(|p| p.age_secs <= HANDOFF_PATH_SECS)
        );
    }

    #[test]
    fn retention_is_independent_of_mouse_report_rate_and_preserves_live_head() {
        let count = |hz: u32| {
            let mut e = Effect::new(Params::default(), 1);
            e.set_trail_duration(10.);
            e.set_active(0., true);
            for i in 0..=hz * 12 {
                let t = f64::from(i) / f64::from(hz);
                e.push(t, Point::new(t * 100., 0.));
                assert_eq!(e.path.back().unwrap().1, Point::new(t * 100., 0.));
            }
            assert!(e.path.front().unwrap().0 <= 2.001);
            e.path.len()
        };
        let counts = [count(1000), count(8000)];
        assert!(counts[0].abs_diff(counts[1]) <= 8, "{counts:?}");
        assert!(counts[0] < 320);
    }

    #[test]
    fn fast_retina_circle_stays_within_half_a_physical_pixel() {
        let mut e = Effect::new(Params::default(), 1);
        e.set_trail_duration(1.);
        e.set_active(0., true);
        let point = |t: f64| {
            Point::new(
                1200. * (t * std::f64::consts::TAU / 0.3).cos(),
                1200. * (t * std::f64::consts::TAU / 0.3).sin(),
            )
        };
        for i in 0..=300 {
            e.set_display_scale(1.);
            e.push(f64::from(i) / 1000., point(f64::from(i) / 1000.));
        }
        assert!(e.path.len() < 200);
        for window in e.path.make_contiguous().windows(2) {
            let [(ta, a), (tb, b)] = window else { unreachable!() };
            for i in 0..=10 {
                let u = f64::from(i) / 10.;
                let truth = point(ta + (tb - ta) * u);
                let p = Point::new(a.x + (b.x - a.x) * u, a.y + (b.y - a.y) * u);
                assert!((p.x - truth.x).hypot(p.y - truth.y) < 0.5);
            }
        }
    }

    #[test]
    fn all_circle_cases_keep_subpixel_geometry_and_worst_case_fits_sixty_seconds() {
        for r in [50., 100., 300., 600.] {
            for period in [0.3, 0.5, 1.] {
                for scale in [1., 2.] {
                    let mut e = Effect::new(Params::default(), 1);
                    e.set_trail_duration(60.);
                    e.set_display_scale(scale);
                    e.set_active(0., true);
                    let point = |t: f64| {
                        Point::new(
                            r * (t * std::f64::consts::TAU / period).cos(),
                            r * (t * std::f64::consts::TAU / period).sin(),
                        )
                    };
                    for i in 0..=3000 {
                        e.push(f64::from(i) / 1000., point(f64::from(i) / 1000.));
                    }
                    for window in e.path.make_contiguous().windows(2) {
                        let [(ta, a), (tb, b)] = window else { unreachable!() };
                        for i in 0..=10 {
                            let u = f64::from(i) / 10.;
                            let truth = point(ta + (tb - ta) * u);
                            let p = Point::new(a.x + (b.x - a.x) * u, a.y + (b.y - a.y) * u);
                            assert!(
                                (p.x - truth.x).hypot(p.y - truth.y) * scale <= 0.501,
                                "R={r} T={period} scale={scale}"
                            );
                        }
                    }
                    // Max observed case retains ~500/s, so 60.05 seconds fits.
                    assert!((e.path.len() as f64 / 3. * 60.05) < MAX_PATH_POINTS as f64);
                }
            }
        }
    }

    #[test]
    fn configured_duration_retains_full_decimated_history_and_controls_idle() {
        for ms in [100, 270, 10_000, 60_000] {
            let secs = f64::from(ms) / 1000.;
            let mut e = Effect::new(Params::default(), 1);
            e.set_trail_duration(secs);
            e.set_active(0., true);
            for i in 0..=61_000 {
                e.push(f64::from(i) / 1000., Point::new(f64::from(i), 0.));
            }
            let count = e.ribbon_count(61.);
            assert!(
                count >= (secs / PATH_MAX_GAP_SECS).floor() as usize,
                "{ms}: only {count} points"
            );
            assert!(e.path.len() <= MAX_PATH_POINTS);
            assert!(e.path.len() <= ((secs + 0.05) / (PATH_MAX_GAP_SECS - 0.001)).ceil() as usize + 2);
            let state = e.continuation(61.).unwrap();
            assert!(state.valid());
            assert!(state.points.first().unwrap().age_secs >= secs.min(HANDOFF_PATH_SECS) - PATH_MAX_GAP_SECS);
            assert!(state.points.iter().all(|p| p.age_secs <= HANDOFF_PATH_SECS));
            assert!(state.points.len() < MAX_TRAIL_POINTS);
            e.set_active(61., false);
            let fade = secs.max(0.525);
            assert!(e.alive(61. + fade - 0.001));
            assert!(!e.alive(61. + fade + 0.001));
            if secs > 0.525 {
                assert!(e.ribbon_count(61. + secs - 0.001) > 0);
            }
            assert_eq!(e.ribbon_count(61. + secs + 0.001), 0);
            let mut departed = Effect::new(*e.params(), 1);
            departed.set_active(0., true);
            departed.push(1., Point::new(1., 0.));
            departed.depart(1., Point::new(1., 0.));
            assert!(departed.alive(1. + secs - 0.001));
            assert!(!departed.alive(1. + secs + 0.001));
        }
    }

    use super::*;
    use std::f64::consts::TAU;

    #[test]
    fn virtual_gap_head_is_clipped_without_changing_history_or_timing() {
        let mut e = Effect::new(
            Params {
                fade_in: 0.,
                emit_rate: 0.,
                ..Params::default()
            },
            1,
        );
        e.set_cell(1., 0.25 / 686.);
        e.set_active(0., true);
        e.push(0., Point::new(90., 50.));
        e.push(0.05, Point::new(99., 50.));
        e.push(0.1, Point::new(105., 50.));
        let state = e.continuation(0.1).unwrap();
        e.set_clip(Some(Rect::new(0., 0., 100., 100.)));
        assert_eq!(e.continuation(0.1).unwrap(), state);
        let region = CellRect {
            x: 80,
            y: 40,
            w: 40,
            h: 20,
        };
        let mut pixels = Vec::new();
        e.render(0.1, region, &mut pixels);
        assert!(pixels.iter().enumerate().any(|(i, p)| i % 40 < 20 && *p != 0));
        assert!(pixels.iter().enumerate().all(|(i, p)| i % 40 < 20 || *p == 0));
        let bounds = e.bounds(0.1).unwrap();
        assert!(bounds.x + i64::from(bounds.w) <= 100);
        e.push(0.4, Point::new(150., 50.));
        assert!(e.bounds(0.4).is_none(), "old on-screen history aged out during travel");
    }

    /// 格子 1 本机单位 = 0.2/686 reference units（16 寸 Mac：1 pt）。
    const CELL_REFERENCE: f64 = 0.2 / 686.0;

    /// 半径 60 单位（12/686 reference units）、2.5 圈/秒的圈，120 Hz。
    fn circle_effect(active_secs: f64) -> (Effect, f64) {
        let mut e = Effect::new(Params::default(), 42);
        e.set_cell(1.0, CELL_REFERENCE);
        let mut t = 0.0;
        e.set_active(0.0, true);
        while t < active_secs {
            e.push(t, circle_pos(t));
            e.step(t);
            t += 1.0 / 120.0;
        }
        (e, t - 1.0 / 120.0)
    }

    fn circle_pos(t: f64) -> Point {
        let a = TAU * 2.5 * t;
        Point::new(500.0 + 60.0 * a.cos(), 400.0 + 60.0 * a.sin())
    }

    fn alpha(px: u32) -> u32 {
        px >> 24
    }

    fn ribbon_only() -> Params {
        Params {
            emit_rate: 0.0,
            ..Params::default()
        }
    }

    /// 某个位置（本机单位 = 格）附近 3×3 格里的最大 alpha。
    fn alpha_near(out: &[u32], b: CellRect, p: Point) -> u32 {
        let gx = p.x.floor() as i64 - b.x;
        let gy = p.y.floor() as i64 - b.y;
        let mut m = 0;
        for dy in -1..=1 {
            for dx in -1..=1 {
                let (x, y) = (gx + dx, gy + dy);
                if x >= 0 && y >= 0 && x < i64::from(b.w) && y < i64::from(b.h) {
                    m = m.max(alpha(out[y as usize * b.w as usize + x as usize]));
                }
            }
        }
        m
    }

    #[test]
    fn continuation_is_bounded_bright_immediately_and_keeps_emission_phase() {
        let (source, t) = circle_effect(1.0);
        let state = source.continuation(t).unwrap();
        assert!(state.points.len() >= 2 && state.points.len() < MAX_TRAIL_POINTS);
        let mut target = Effect::new(Params::default(), 999);
        target.resume(100.0, &state); // unrelated clocks
        assert_eq!(target.envelope(100.0), 1.0);
        assert_eq!(target.emit_carry, source.emit_carry);
        assert_eq!(target.heading, source.heading);
        let b = target.bounds(100.0).unwrap();
        let mut pixels = Vec::new();
        target.render(100.0, b, &mut pixels);
        assert!(pixels.iter().filter(|p| **p >> 24 > 0).count() > 200);
        let n = target.particle_count();
        let (_, last) = target.path.back().copied().unwrap();
        target.push(100.04, Point::new(last.x + 10., last.y));
        assert!(
            target.particle_count() > n,
            "emitter starts immediately, no fresh fade-in"
        );
    }

    #[test]
    fn departure_uses_natural_lifetimes_without_whole_effect_fade() {
        let (mut source, t) = circle_effect(1.0);
        let exit = source.path.back().unwrap().1;
        let last_death = source.particles.iter().map(|p| p.born + p.life).fold(t, f64::max);
        let count = source.ribbon_points(t).count();
        source.depart(t, exit);
        assert!(!source.active());
        assert_eq!(source.envelope(t + 0.2), 1.0);
        assert!(source.ribbon_points(t + 0.15).count() < count);
        assert_eq!(source.ribbon_points(t + 0.271).count(), 0);
        assert!(last_death > t + source.params().fade_out);
        assert!(source.alive(last_death - 1e-6));
        source.step(last_death + 1e-6);
        assert_eq!(source.particle_count(), 0);
        assert!(!source.alive(last_death + 1e-6));
        assert!(source.finish_if_done(last_death + 1e-6));
    }

    #[test]
    fn calibrated_sizes_are_continuous_in_both_directions_and_on_fast_return() {
        for ratio in [0.67, 1.0 / 0.67, 1.0] {
            let (source, t) = circle_effect(1.0);
            let mut state = source.continuation(t).unwrap();
            // Routing converts old normal units/plane by source layout plane/unit
            // divided by destination layout plane/unit.
            state.native_units_per_reference *= ratio;
            let mut target = Effect::new(Params::default(), 5);
            target.resume(10.0, &state);
            assert!((target.size_scale(10.0) - ratio).abs() < 1e-12);
            let mut previous = target.size_scale(10.0);
            for i in 1..=40 {
                let now = target.size_scale(10.0 + f64::from(i) / 100.0);
                if ratio < 1.0 {
                    assert!(now >= previous);
                } else {
                    assert!(now <= previous);
                }
                previous = now;
            }
            assert_eq!(target.size_scale(10.4), 1.0);
            let during = target.size_scale(10.15);
            let mut back = target.continuation(10.15).unwrap();
            assert_eq!(back.size_scale, during);
            back.native_units_per_reference /= ratio;
            let mut returned = Effect::new(Params::default(), 6);
            returned.resume(200.0, &back);
            assert!((returned.size_scale(200.0) * ratio - during).abs() < 1e-12);
            assert_eq!(returned.size_scale(201.0), 1.0);
            // Physical/plane head radius agrees across the seam.
            let old_radius = source.params().head_radius * (source.cell / source.cell_reference) * ratio;
            let new_radius =
                target.params().head_radius * (target.cell / target.cell_reference) * target.size_scale(10.0);
            assert!((old_radius - new_radius).abs() < 1e-12);
        }
    }

    #[test]
    fn the_same_scale_applies_to_existing_and_new_particle_geometry() {
        let (mut e, t) = circle_effect(1.0);
        let p = *e.particles.back().unwrap();
        let before = e.particle_at(&p, t).unwrap();
        e.size_transition = Some((t, 0.67));
        let after = e.particle_at(&p, t).unwrap();
        assert!((after.r / before.r - 0.67).abs() < 1e-12);
        assert!(((after.cx - p.x / e.cell) / (before.cx - p.x / e.cell) - 0.67).abs() < 1e-10);
        let previous = e.particle_count();
        e.push(t + 0.04, Point::new(550., 410.));
        assert!(e.particle_count() > previous);
        let newborn = *e.particles.back().unwrap();
        let actual = e.particle_at(&newborn, t + 0.05).unwrap();
        e.size_transition = None;
        let normal = e.particle_at(&newborn, t + 0.05).unwrap();
        assert!((actual.r / normal.r - (0.67 + 0.33 * smooth(0.05 / 0.4))).abs() < 1e-12);
    }

    #[test]
    fn draws_a_bright_head_and_a_faint_tail() {
        let (e, t) = circle_effect(1.0);
        let b = e.bounds(t).expect("visible");
        let mut out = Vec::new();
        e.render(t, b, &mut out);
        let lit = out.iter().filter(|p| alpha(**p) > 0).count();
        assert!(lit > 200, "lit cells {lit}");
        let head = e.path.back().unwrap().1;
        let old = circle_pos(t - 0.16);
        assert!(
            alpha_near(&out, b, head) > alpha_near(&out, b, old) + 80,
            "head {} old {}",
            alpha_near(&out, b, head),
            alpha_near(&out, b, old)
        );
        assert!(alpha_near(&out, b, head) > 200);
        // 有粒子，而且下一帧不一样（粒子在飞）。
        assert!(e.particle_count() > 5, "{} particles", e.particle_count());
        let mut next = Vec::new();
        e.render(t + 0.03, b, &mut next);
        assert_ne!(out, next);
        // Default accent is pink, with blue between green and red.
        let (mut r, mut g, mut bl) = (0u64, 0u64, 0u64);
        for p in &out {
            r += u64::from((p >> 16) & 0xFF);
            g += u64::from((p >> 8) & 0xFF);
            bl += u64::from(p & 0xFF);
        }
        assert!(r > g * 3 / 2 && r > bl && bl > g, "r {r} g {g} b {bl}");
    }

    /// 沿垂直于运动方向的一条线数有多少格被彗尾覆盖（直线运动，向 +x）。
    #[test]
    fn the_tail_narrows_quickly_behind_the_pointer() {
        let mut e = Effect::new(ribbon_only(), 7);
        e.set_cell(1.0, CELL_REFERENCE);
        e.set_active(0.0, true);
        // 300/686 reference units/s 向右 = 1500 格/秒。
        let mut t = 0.0;
        while t <= 0.3 {
            e.push(t, Point::new(100.0 + 1500.0 * t, 200.0));
            t += 1.0 / 240.0;
        }
        let t = t - 1.0 / 240.0;
        let head_x = 100.0 + 1500.0 * t;
        let b = e.bounds(t).unwrap();
        let mut out = Vec::new();
        e.render(t, b, &mut out);
        let width_at = |x: f64| {
            let gx = x.floor() as i64 - b.x;
            (0..b.h as usize)
                .filter(|&y| alpha(out[y * b.w as usize + gx as usize]) > 8)
                .count()
        };
        let head_r = Params::default().head_radius / CELL_REFERENCE;
        let at_head = width_at(head_x - 1.0);
        // 指针处约 2 × 头部半径宽。
        assert!((at_head as f64) > 1.6 * head_r, "head width {at_head}");
        // 寿命延长后，在相同的长度比例处仍快速收窄。
        let mid = width_at(head_x - 1500.0 * e.p.trail_secs * (0.07 / 0.18));
        let late = width_at(head_x - 1500.0 * e.p.trail_secs * (0.15 / 0.18));
        assert!(mid * 2 < at_head, "head {at_head} mid {mid}");
        assert!(late <= 3, "late {late}");
        // 越往后越淡。
        let a = |x: f64| alpha_near(&out, b, Point::new(x, 200.0));
        assert!(a(head_x - 2.0) > a(head_x - 1500.0 * e.p.trail_secs * (0.07 / 0.18)));
        assert!(
            a(head_x - 1500.0 * e.p.trail_secs * (0.07 / 0.18)) > a(head_x - 1500.0 * e.p.trail_secs * (0.15 / 0.18))
        );
    }

    #[test]
    fn particles_fly_backwards_in_a_fan_and_slow_down() {
        let p = Params::default();
        let mut e = Effect::new(p, 3);
        e.set_cell(1.0, CELL_REFERENCE);
        e.set_active(0.0, true);
        let mut t = 0.0;
        while t <= 0.5 {
            e.push(t, Point::new(100.0 + 1500.0 * t, 200.0));
            t += 1.0 / 120.0;
        }
        let n = e.particle_count();
        // 30 个/秒 × 0.5 秒（这里没调用 step，全部还在）。
        assert!((12..=18).contains(&n), "{n} particles");
        let half_fan = (p.fan_deg / 2.0).to_radians() + 1e-9;
        for q in &e.particles {
            // 运动方向是 +x：速度方向在以 −x 为中心的扇形内。
            let ang = q.vy.atan2(-q.vx).abs();
            assert!(
                ang <= half_fan,
                "particle at {:.1} deg from straight back",
                ang.to_degrees()
            );
            let speed_reference = q.vx.hypot(q.vy) * CELL_REFERENCE;
            assert!(
                (p.particle_speed.0 - 1e-9..=p.particle_speed.1 + 1e-9).contains(&speed_reference),
                "{speed_reference}"
            );
        }
        // 减速：后半段寿命里走的路比前半段少得多。
        let q = *e.particles.front().unwrap();
        let pos = |age: f64| e.particle_at(&q, q.born + age).map(|n| (n.cx, n.cy));
        let (p0, p1, p2) = (pos(0.0).unwrap(), pos(0.14).unwrap(), pos(0.28).unwrap());
        let d1 = (p1.0 - p0.0).hypot(p1.1 - p0.1);
        let d2 = (p2.0 - p1.0).hypot(p2.1 - p1.1);
        assert!(d2 < d1 * 0.6, "first {d1:.2} then {d2:.2}");
        // 淡出、缩小，寿命结束后消失。
        let early = e.particle_at(&q, q.born + 0.05).unwrap();
        let late = e.particle_at(&q, q.born + q.life * 0.9).unwrap();
        assert!(late.alpha < early.alpha * 0.2 && late.r < early.r);
        assert!(e.particle_at(&q, q.born + q.life).is_none());
        e.step(q.born + q.life + 0.01);
        assert!(e.particle_count() < n);
    }

    #[test]
    fn particles_are_round_and_small() {
        let mut e = Effect::new(Params::default(), 5);
        e.set_cell(1.0, CELL_REFERENCE);
        e.set_active(0.0, true);
        e.particles.push_back(Particle {
            born: 0.0,
            x: 50.3,
            y: 40.7,
            vx: 0.0,
            vy: 0.0,
            life: 1.0,
            radius: 2.0,
            brightness: 1.0,
            white: 0.5,
        });
        let region = CellRect {
            x: 40,
            y: 30,
            w: 21,
            h: 21,
        };
        let mut out = Vec::new();
        e.render(0.1, region, &mut out);
        let a = |x: i64, y: i64| alpha(out[((y - 30) * 21 + (x - 40)) as usize]);
        // 圆心附近满、外接方块的角上没有（不是方块）、四个方向对称。
        assert!(a(50, 40) > 150);
        assert_eq!(a(48, 38), 0, "corner of the bounding square");
        assert_eq!(a(52, 43), 0);
        let lit = out.iter().filter(|p| alpha(**p) > 0).count();
        assert!((10..=24).contains(&lit), "lit {lit}");
        // 默认粒子比上一版的方形星点（1–3 格 × 0.45/686 reference units）小：直径不到 1/686 reference units。
        assert!(Params::default().particle_radius.1 * 2.0 < 1.0);
    }

    #[test]
    fn premultiplied_output_never_exceeds_alpha() {
        let (e, t) = circle_effect(0.8);
        let b = e.bounds(t).unwrap();
        let mut out = Vec::new();
        e.render(t, b, &mut out);
        for p in out {
            let a = p >> 24;
            assert!(
                (p >> 16) & 0xFF <= a && (p >> 8) & 0xFF <= a && p & 0xFF <= a,
                "{p:08x}"
            );
        }
    }

    #[test]
    fn fades_out_after_stopping_and_then_is_done() {
        let (mut e, t) = circle_effect(0.8);
        e.set_active(t, false);
        assert!(e.alive(t + 0.4));
        assert!(e.envelope(t + 0.4) < 0.5);
        assert!(!e.alive(t + 0.526));
        assert_eq!(e.bounds(t + 0.526), None);
        assert!(e.finish_if_done(t + 0.526));
        assert!(!e.alive(t + 1.0));
        // 结束后可以换格子大小。
        e.set_cell(3.0, 0.48 / 686.0);
        assert_eq!(e.cell(), 3.0);
    }

    #[test]
    fn restarting_during_fade_out_continues_from_current_brightness() {
        let (mut e, t) = circle_effect(0.8);
        e.set_active(t, false);
        let before = e.envelope(t + 0.1);
        e.set_active(t + 0.1, true);
        let after = e.envelope(t + 0.1);
        assert!((before - after).abs() < 0.2, "{before} vs {after}");
        assert!(e.active());
    }

    #[test]
    fn regions_split_seamlessly() {
        let (e, t) = circle_effect(0.9);
        let b = e.bounds(t).unwrap();
        let mut whole = Vec::new();
        e.render(t, b, &mut whole);
        // 左右两半分别画（两块屏幕的接缝），拼起来与整体一致。
        let left = CellRect { w: b.w / 2, ..b };
        let right = CellRect {
            x: b.x + i64::from(b.w / 2),
            w: b.w - b.w / 2,
            ..b
        };
        let (mut l, mut r) = (Vec::new(), Vec::new());
        e.render(t, left, &mut l);
        e.render(t, right, &mut r);
        for y in 0..b.h as usize {
            for x in 0..b.w as usize {
                let v = if x < left.w as usize {
                    l[y * left.w as usize + x]
                } else {
                    r[y * right.w as usize + x - left.w as usize]
                };
                assert_eq!(v, whole[y * b.w as usize + x]);
            }
        }
    }

    #[test]
    fn pointer_jumps_are_not_connected() {
        let mut e = Effect::new(Params::default(), 1);
        e.set_cell(2.0, 0.4 / 686.0);
        e.set_active(0.0, true);
        e.push(0.0, Point::new(0.0, 0.0));
        e.push(0.01, Point::new(5.0, 0.0));
        e.push(0.02, Point::new(2000.0, 0.0));
        e.push(0.03, Point::new(2005.0, 0.0));
        let b = e.bounds(0.03).unwrap();
        let mut out = Vec::new();
        e.render(0.03, b, &mut out);
        // 中间（x = 1000 附近）没有内容。
        let gx = (1000.0 / 2.0) as i64 - b.x;
        for y in 0..b.h as usize {
            assert_eq!(out[y * b.w as usize + gx as usize], 0);
        }
    }

    #[test]
    fn bilinear_upscale_is_smooth_and_keeps_premultiplied_colors_valid() {
        // 3×1: transparent, opaque accent, transparent.
        let accent = over(0, Palette::new(Params::default().trail_color).head, 1.0);
        let src = [0, accent, 0];
        let k = 4;
        let mut out = vec![0u32; 12 * 4];
        upscale_bilinear(&src, 3, 1, k, &mut out, 12);
        // 第 2 行（格子中心附近）：中间格子的中心最不透明，向两边平滑下降（不是 4 个一样的像素一跳）。
        let row: Vec<u32> = out[12..24].iter().map(|p| alpha(*p)).collect();
        assert!(row[5] == row[6] && row[5] > 180, "{row:?}");
        assert!(row[4] < row[5] && row[3] < row[4] && row[2] < row[3], "{row:?}");
        // 上下对称。
        assert_eq!(out[..12], out[36..48]);
        assert_eq!(out[12..24], out[24..36]);
        for p in &out {
            let a = p >> 24;
            assert!(
                (p >> 16) & 0xFF <= a && (p >> 8) & 0xFF <= a && p & 0xFF <= a,
                "{p:08x}"
            );
        }
        // k = 1 是复制。
        let mut same = vec![0u32; 3];
        upscale_bilinear(&src, 3, 1, 1, &mut same, 3);
        assert_eq!(same, src);
    }

    #[test]
    fn intersect_rects() {
        let a = CellRect {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        };
        let b = CellRect {
            x: 5,
            y: -5,
            w: 10,
            h: 8,
        };
        assert_eq!(a.intersect(&b), CellRect { x: 5, y: 0, w: 5, h: 3 });
        assert!(
            a.intersect(&CellRect {
                x: 20,
                y: 0,
                w: 1,
                h: 1
            })
            .is_empty()
        );
    }
}
