//! 光标定位拖尾的绘制。
//!
//! 何时画由输入路由决定（源端识别快速画圈，指针所在的设备输出 `Output::CursorTrail`）；本 crate 只负责
//! 在本机屏幕上沿本机光标的轨迹画出来：
//!
//! - [`effect`]：效果模型与光栅化（两个平台共用一份，像素一致）；
//! - 平台层：macOS 为每块屏幕一个透明、鼠标穿透、不激活、在全屏应用之上的 `NSPanel`，内容是一个按格子
//!   双线性放大的 `CALayer`；Windows 为一个跟着拖尾范围移动的分层窗口（`UpdateLayeredWindow`，逐像素
//!   alpha，`WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOPMOST`）。
//!
//! [`CursorTrail`] 是可在任意线程调用的句柄：[`CursorTrail::pointer`] 在截获回调里调用，只把点追加到一个
//! 有界的纯模型；绘制在专门的线程里，只在显示或淡出期间按屏幕刷新率出帧，其余时间睡在条件变量上（零开销）。

pub mod aim;
pub mod diagnostics;
pub mod effect;
pub mod projection;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use oink_foundation::Point;
use oink_foundation::display::DisplayInfo;
use oink_routing::control::TrailContinuation;

pub use effect::{CellRect, Effect, Params, upscale_bilinear};

/// 一个格子（光栅化的分辨率）的目标物理边长（plane）。16 寸 Mac 上 2 个设备像素（1 pt，平台层双线性放大）、
/// 4K 28 寸屏上 1 个像素。
pub const CELL_REFERENCE: f64 = 0.2 / 686.0;

/// 显示器上一个格子的边长（本机单位）与它的物理边长（plane）：取整到整数个设备像素，至少 1 个。
/// 不知道物理尺寸时按 1 个本机单位、[`effect::DEFAULT_CELL_REFERENCE`] 算。
pub fn cell_for(d: &DisplayInfo) -> (f64, f64) {
    let ppu = d.pixels_per_unit().max(0.5);
    match d.reference_per_native_unit() {
        Some(mpu) if mpu > 0.0 => {
            let px = (CELL_REFERENCE / mpu * ppu).round().max(1.0);
            let cell = px / ppu;
            (cell, cell * mpu)
        }
        _ => (1.0, effect::DEFAULT_CELL_REFERENCE),
    }
}

/// 一次显示（从开始到淡出结束）的绘制统计。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SessionStats {
    pub frames: u64,
    pub secs: f64,
    /// 每帧在绘制线程上的耗时（模型、光栅化、交给平台）。
    pub avg_frame_ms: f64,
    pub max_frame_ms: f64,
    /// 本次最大的一帧画了多少个格子（所有屏幕合计）。
    pub max_cells: u64,
    pub max_particles: usize,
}

/// 累计统计。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TrailStats {
    pub sessions: u64,
    pub frames: u64,
    pub last: Option<SessionStats>,
}

#[derive(Debug)]
struct Input {
    paused: bool,
    pause_ack: bool,
    model: Effect,
    retired: Vec<Effect>,
    active: bool,
    suppressed: bool,
    displays: Vec<DisplayInfo>,
    displays_changed: bool,
    thread_started: bool,
    report_first: bool,
    retirement_report_at: f64,
    stop: bool,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            paused: false,
            pause_ack: false,
            model: Effect::new(Params::default(), 0x5DEE_CE66_D1CE_4E5B),
            retired: Vec::new(),
            active: false,
            suppressed: false,
            displays: Vec::new(),
            displays_changed: false,
            thread_started: false,
            report_first: false,
            retirement_report_at: 0.,
            stop: false,
        }
    }
}

impl Input {
    fn set_emitting(&mut self, t: f64, want: bool) {
        if want && !self.model.active() {
            // A fresh local session cannot composite a previous session's tails.
            self.retired.clear();
        }
        self.model.set_active(t, want);
    }

    fn resume(&mut self, t: f64, state: &TrailContinuation, clip: Option<oink_foundation::Rect>) {
        self.retired.clear();
        self.model = Effect::new(*self.model.params(), (t.to_bits() ^ 0x5DEE_CE66_D1CE_4E5B) | 1);
        if let Some(p) = state.points.last() {
            self.cell_at(p.pos);
        }
        self.model.resume(t, state);
        self.model.set_clip(clip);
    }

    fn retire(&mut self, t: f64) {
        let next = Effect::new(*self.model.params(), (t.to_bits() ^ 0x5DEE_CE66_D1CE_4E5B) | 1);
        let old = std::mem::replace(&mut self.model, next);
        self.retired.retain(|e| e.alive(t));
        if old.alive(t) {
            // Bound work even under pathological back-and-forth warping.
            if self.retired.len() == 16 {
                self.retired.remove(0);
            }
            self.retired.push(old);
        }
    }

    fn cell_at(&mut self, p: Point) {
        if let Some(d) = display_at(&self.displays, p) {
            let (cell, plane) = cell_for(d);
            self.model.set_cell(cell, plane);
            self.model.set_display_scale(d.pixels_per_unit());
        }
    }
}

#[derive(Debug)]
struct Shared {
    health: Arc<diagnostics::Health>,
    input: Mutex<Input>,
    cv: Condvar,
    origin: Instant,
    stats: Mutex<TrailStats>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn secs(&self, at: Instant) -> f64 {
        at.saturating_duration_since(self.origin).as_secs_f64()
    }
}

/// 光标定位拖尾的句柄。可以廉价克隆，在任意线程调用。
#[derive(Clone, Debug)]
pub struct CursorTrail {
    shared: Arc<Shared>,
}

impl CursorTrail {
    /// 本机平台的实现；平台没有实现时为 `None`。
    pub fn platform() -> Option<Self> {
        if !platform::available() {
            return None;
        }
        Some(Self {
            shared: Arc::new(Shared {
                health: diagnostics::Health::new(diagnostics::Kind::Trail),
                input: Mutex::new(Input::default()),
                cv: Condvar::new(),
                origin: Instant::now(),
                stats: Mutex::new(TrailStats::default()),
            }),
        })
    }

    /// Clear all particles and park this renderer. A separate aim-game renderer
    /// is independent and deliberately remains active during group pause.
    pub fn set_paused(&self, paused: bool) {
        let mut input = lock(&self.shared.input);
        input.paused = paused;
        input.pause_ack = !input.thread_started;
        if paused {
            self.shared.health.hide(diagnostics::Hide::Paused);
            input.active = false;
            input.model = Effect::new(*input.model.params(), 0x5DEE_CE66_D1CE_4E5B);
            input.retired.clear();
        }
        self.shared
            .health
            .intent(u64::from(input.active) | (u64::from(paused) << 1) | (u64::from(input.suppressed) << 2));
        self.shared.cv.notify_all();
        while paused && input.thread_started && !input.pause_ack {
            input = self.shared.cv.wait(input).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Apply the configured lifetime to current and departing tails; the renderer's
    /// alive predicate controls redraws, window hiding and idle parking.
    pub fn set_trail_duration(&self, secs: f64) {
        let mut input = lock(&self.shared.input);
        input.model.set_trail_duration(secs);
        for effect in &mut input.retired {
            effect.set_trail_duration(secs);
        }
        self.shared.cv.notify_one();
    }

    /// Rendering-device local accent, applied to active and departing tails on
    /// both platforms before their shared premultiplied-BGRA frame is presented.
    pub fn set_trail_color(&self, rgb: [u8; 3]) {
        let mut input = lock(&self.shared.input);
        input.model.set_trail_color(rgb);
        for effect in &mut input.retired {
            effect.set_trail_color(rgb);
        }
        self.shared.cv.notify_one();
    }

    pub fn trail_color(&self) -> [u8; 3] {
        lock(&self.shared.input).model.params().trail_color
    }

    /// 路由要求开始 / 停止画（`Output::CursorTrail`）。
    pub fn set_active(&self, active: bool) {
        let mut inp = lock(&self.shared.input);
        self.shared
            .health
            .intent(u64::from(active) | (u64::from(inp.paused) << 1) | (u64::from(inp.suppressed) << 2));
        if inp.paused {
            return;
        }
        inp.active = active;
        let t = self.shared.secs(Instant::now());
        let want = active && !inp.suppressed;
        inp.set_emitting(t, want);
        if active && !inp.thread_started {
            inp.thread_started = true;
            let shared = self.shared.clone();
            if let Err(e) = std::thread::Builder::new()
                .name("oink-cursor-trail".into())
                .spawn(move || render_thread(shared))
            {
                tracing::warn!(%e, "cannot start the cursor trail thread");
                self.shared.health.failed();
                inp.thread_started = false;
            }
        }
        self.shared.cv.notify_one();
    }

    /// Exact emission state at the seam. Only bounded model work under the lock;
    /// rasterization and window operations never hold it.
    pub fn continuation(&self, exit: Point) -> Option<TrailContinuation> {
        let t = self.shared.secs(Instant::now());
        let mut inp = lock(&self.shared.input);
        inp.model.push(t, exit);
        inp.model.continuation(t)
    }

    pub fn resume(&self, state: TrailContinuation) {
        self.resume_with_clip(state, None);
    }

    /// Apply the clip atomically with continuation, before the first frame.
    pub fn resume_with_clip(&self, state: TrailContinuation, clip: Option<oink_foundation::Rect>) {
        if !state.valid() {
            return;
        }
        {
            let t = self.shared.secs(Instant::now());
            let mut inp = lock(&self.shared.input);
            inp.resume(t, &state, clip);
            inp.report_first = true;
            // FocusEnter restores the cursor; the last sample can still describe
            // Oink's remote-mode hidden cursor.
            inp.suppressed = false;
            tracing::info!(
                points = state.points.len(),
                envelope = inp.model.envelope(t),
                carry = state.emit_carry,
                size_scale = inp.model.size_scale(t),
                "cursor trail resumed first frame state"
            );
        }
        self.set_active(true);
    }

    pub fn depart(&self, exit: Point) {
        let t = self.shared.secs(Instant::now());
        let mut inp = lock(&self.shared.input);
        inp.model.depart(t, exit);
        tracing::info!(
            particles = inp.model.particle_count(),
            envelope = inp.model.envelope(t),
            "cursor trail departed"
        );
        inp.retire(t);
        inp.active = false;
        self.shared.cv.notify_one();
    }

    /// 本机光标被程序隐藏（游戏、视频、打字时）：期间不画。
    pub fn set_suppressed(&self, hidden: bool) {
        let mut inp = lock(&self.shared.input);
        if inp.suppressed != hidden {
            inp.suppressed = hidden;
            self.shared
                .health
                .intent(u64::from(inp.active) | (u64::from(inp.paused) << 1) | (u64::from(hidden) << 2));
            let want = inp.active && !hidden;
            inp.set_emitting(self.shared.secs(Instant::now()), want);
            self.shared.cv.notify_one();
        }
    }

    /// Update the small pure model. No rasterization or platform calls here.
    pub fn pointer(&self, pos: Point) {
        self.pointer_on_display(pos, pos, None);
    }

    /// The head may be in a physical layout gap. Use the visible pointer to
    /// choose the leaving display's grid; virtual coordinates must not select
    /// an unrelated display in the OS desktop arrangement.
    pub fn pointer_on_display(&self, pos: Point, visible: Point, clip: Option<oink_foundation::Rect>) {
        let t = self.shared.secs(Instant::now());
        let mut inp = lock(&self.shared.input);
        inp.model.finish_if_done(t);
        inp.cell_at(visible);
        inp.model.set_clip(clip);
        inp.model.push(t, pos);
    }

    /// 本机显示器变化（启动时也调用一次）。
    pub fn set_displays(&self, displays: Vec<DisplayInfo>) {
        self.shared
            .health
            .event(diagnostics::Event::Displays, displays.len() as u64);
        let mut inp = lock(&self.shared.input);
        inp.displays = displays;
        inp.displays_changed = true;
        self.shared.cv.notify_one();
    }

    /// 绘制统计（性能测量用）。
    pub fn stats(&self) -> TrailStats {
        *lock(&self.shared.stats)
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // 句柄全部释放时绘制线程也已退出（它持有一个 Arc），这里不需要做什么。
    }
}

/// 关闭绘制线程（测试用）：句柄与线程都释放后线程退出。
impl CursorTrail {
    pub fn shutdown(&self) {
        let mut inp = lock(&self.shared.input);
        inp.stop = true;
        self.shared.cv.notify_one();
    }
}

fn display_at(displays: &[DisplayInfo], p: Point) -> Option<&DisplayInfo> {
    displays
        .iter()
        .find(|d| d.bounds.contains(p))
        .or_else(|| displays.iter().find(|d| d.is_primary))
        .or_else(|| displays.first())
}

fn render_thread(shared: Arc<Shared>) {
    let _worker = shared.health.worker();
    let Some(mut backend) = platform::Backend::new(shared.health.clone()) else {
        shared.health.failed();
        let mut input = lock(&shared.input);
        input.thread_started = false;
        input.pause_ack = true;
        shared.cv.notify_all();
        tracing::warn!("cursor trail: no platform renderer");
        return;
    };
    let mut buf = Vec::new();
    let mut session: Option<(Instant, SessionStats)> = None;
    let mut shown = false;
    loop {
        let (mut effects, new_displays, report_first, frame_start) = {
            let mut inp = lock(&shared.input);
            loop {
                if inp.stop {
                    shared.health.hide(diagnostics::Hide::Shutdown);
                    backend.hide();
                    return;
                }
                if inp.paused {
                    backend.hide();
                    shown = false;
                    inp.pause_ack = true;
                    shared.cv.notify_all();
                    inp = shared.cv.wait(inp).unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                let now = shared.secs(Instant::now());
                inp.model.step(now);
                inp.model.finish_if_done(now);
                for e in &mut inp.retired {
                    e.step(now);
                }
                if now >= inp.retirement_report_at && !inp.retired.is_empty() {
                    for e in &inp.retired {
                        tracing::info!(
                            age_secs = e.departure_age(now),
                            ribbon_points = e.ribbon_count(now),
                            particles = e.particle_count(),
                            envelope = e.envelope(now),
                            "cursor trail retirement"
                        );
                    }
                    inp.retirement_report_at = now + 0.15;
                }
                inp.retired.retain(|e| {
                    let alive = e.alive(now);
                    if !alive {
                        tracing::info!(age_secs = e.departure_age(now), "cursor trail departure finished");
                    }
                    alive
                });
                if inp.model.alive(now) || !inp.retired.is_empty() {
                    break;
                }
                // 空闲：撤掉覆盖层，睡到下一次要求。
                if shown {
                    shared.health.hide(diagnostics::Hide::Idle);
                    backend.hide();
                    shown = false;
                }
                if let Some((start, mut st)) = session.take() {
                    st.secs = start.elapsed().as_secs_f64();
                    if st.frames > 0 {
                        st.avg_frame_ms /= st.frames as f64;
                    }
                    tracing::info!(
                        frames = st.frames,
                        secs = format!("{:.2}", st.secs),
                        fps = format!("{:.0}", st.frames as f64 / st.secs.max(1e-3)),
                        avg_ms = format!("{:.3}", st.avg_frame_ms),
                        max_ms = format!("{:.3}", st.max_frame_ms),
                        max_cells = st.max_cells,
                        max_particles = st.max_particles,
                        "cursor trail session"
                    );
                    let mut s = lock(&shared.stats);
                    s.sessions += 1;
                    s.last = Some(st);
                }
                inp = shared.cv.wait(inp).unwrap_or_else(|e| e.into_inner());
            }
            let frame_start = Instant::now();
            let mut effects = inp.retired.clone();
            if inp.model.alive(shared.secs(Instant::now())) {
                effects.push(inp.model.clone());
            }
            let displays = if std::mem::take(&mut inp.displays_changed) {
                Some(inp.displays.clone())
            } else {
                None
            };
            (effects, displays, std::mem::take(&mut inp.report_first), frame_start)
        };
        if let Some(d) = new_displays {
            backend.set_displays(&d);
        }
        let t = shared.secs(frame_start);
        session.get_or_insert((frame_start, SessionStats::default()));
        let cell = effects.last().map_or(1.0, Effect::cell);
        for e in &mut effects {
            e.regrid(cell);
        }
        let bounds = effects.iter().filter_map(|e| e.bounds(t)).reduce(union);
        let mut cells = 0u64;
        let mut lit = 0usize;
        if let Some(b) = bounds {
            let regions = backend.regions(b, cell);
            let mut layers = Vec::with_capacity(regions.len());
            for (surface, r) in regions {
                let mut pixels = vec![0; r.w as usize * r.h as usize];
                for (i, e) in effects.iter().enumerate() {
                    if i == 0 {
                        e.render(t, r, &mut pixels);
                    } else {
                        e.render(t, r, &mut buf);
                        for (dst, src) in pixels.iter_mut().zip(&buf) {
                            *dst = over_pixel(*dst, *src);
                        }
                    }
                }
                if report_first {
                    lit += pixels.iter().filter(|p| **p >> 24 > 0).count();
                }
                cells += u64::from(r.w) * u64::from(r.h);
                layers.push(platform::Layer {
                    surface,
                    rect: r,
                    pixels,
                });
            }
            shared.health.frame(layers.len());
            backend.present(cell, layers);
            #[cfg(not(target_os = "macos"))]
            shared.health.applied();
            shown = true;
        } else if shown {
            shared.health.hide(diagnostics::Hide::NoBounds);
            backend.hide();
            shown = false;
        }

        shared.health.pulse();

        if report_first && let Some(e) = effects.last() {
            tracing::info!(
                ribbon_points = e.ribbon_count(t),
                envelope = e.envelope(t),
                size_scale = e.size_scale(t),
                lit_pixels = lit,
                "cursor trail resumed first rendered frame"
            );
        }
        let ms = frame_start.elapsed().as_secs_f64() * 1000.0;
        if let Some((_, st)) = session.as_mut() {
            st.frames += 1;
            st.avg_frame_ms += ms;
            st.max_frame_ms = st.max_frame_ms.max(ms);
            st.max_cells = st.max_cells.max(cells);
            st.max_particles = st.max_particles.max(effects.iter().map(Effect::particle_count).sum());
        }
        lock(&shared.stats).frames += 1;
        backend.wait_frame(frame_start);
    }
}

fn union(a: CellRect, b: CellRect) -> CellRect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    CellRect {
        x,
        y,
        w: (a.x + i64::from(a.w)).max(b.x + i64::from(b.w)).saturating_sub(x) as u32,
        h: (a.y + i64::from(a.h)).max(b.y + i64::from(b.h)).saturating_sub(y) as u32,
    }
}

/// Premultiplied BGRA source-over, shared by both platform renderers.
fn over_pixel(dst: u32, src: u32) -> u32 {
    let inv = 255 - (src >> 24);
    let mut out = 0;
    for shift in [0, 8, 16, 24] {
        let c = ((src >> shift) & 255) + (((dst >> shift) & 255) * inv + 127) / 255;
        out |= c.min(255) << shift;
    }
    out
}

/// 每帧最长间隔：平台不知道刷新率时按 60 Hz。
pub(crate) const DEFAULT_FRAME: Duration = Duration::from_micros(16_667);

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;

    pub(crate) fn available() -> bool {
        false
    }

    pub(crate) struct Layer {
        pub surface: usize,
        pub rect: CellRect,
        pub pixels: Vec<u32>,
    }

    pub(crate) struct Backend;

    impl Backend {
        pub(crate) fn new() -> Option<Self> {
            None
        }
        pub(crate) fn set_displays(&mut self, _: &[DisplayInfo]) {}
        pub(crate) fn regions(&self, _: CellRect, _: f64) -> Vec<(usize, CellRect)> {
            Vec::new()
        }
        pub(crate) fn present(&mut self, _: f64, _: Vec<Layer>) {}
        pub(crate) fn hide(&mut self) {}
        pub(crate) fn wait_frame(&mut self, _: Instant) {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn local_tint_survives_departure_pause_and_foreign_seam_resume() {
        let trail = CursorTrail {
            shared: Arc::new(Shared {
                health: diagnostics::Health::new(diagnostics::Kind::Trail),
                input: Mutex::new(Input::default()),
                cv: Condvar::new(),
                origin: Instant::now(),
                stats: Mutex::default(),
            }),
        };
        trail.set_trail_color([231, 54, 129]);
        {
            let mut input = lock(&trail.shared.input);
            input.model.set_active(0., true);
            input.model.push(0.1, Point::new(1., 0.));
            input.model.depart(0.1, Point::new(1., 0.));
            input.retire(0.1);
            assert_eq!(input.retired[0].params().trail_color, [231, 54, 129]);
            assert_eq!(input.model.params().trail_color, [231, 54, 129]);
        }
        trail.set_trail_color([18, 171, 205]);
        assert_eq!(
            lock(&trail.shared.input).retired[0].params().trail_color,
            [18, 171, 205]
        );
        let mut source = Effect::new(Params::default(), 2);
        source.set_trail_color([255, 0, 0]);
        source.set_active(0., true);
        source.push(0.1, Point::new(10., 10.));
        lock(&trail.shared.input).resume(0.5, &source.continuation(0.1).unwrap(), None);
        assert_eq!(trail.trail_color(), [18, 171, 205]);
        trail.set_paused(true);
        assert_eq!(trail.trail_color(), [18, 171, 205]);
    }

    #[test]
    fn fresh_start_and_arrival_discard_old_local_and_retired_sessions() {
        let mut input = Input::default();
        input.model.set_trail_duration(10.);
        input.set_emitting(0., true);
        input.model.push(0., Point::new(1000., 1000.));
        input.model.push(0.1, Point::new(1010., 1000.));
        input.model.depart(0.1, Point::new(1010., 1000.));
        input.retire(0.1);
        assert_eq!(input.retired.len(), 1);
        input.set_emitting(0.2, true);
        assert!(input.retired.is_empty());
        assert_eq!(input.model.ribbon_count(0.2), 0);

        input.model.push(0.2, Point::new(2000., 1000.));
        input.model.push(0.3, Point::new(2010., 1000.));
        input.model.depart(0.3, Point::new(2010., 1000.));
        input.retire(0.3);
        input.model.set_active(0.4, true);
        input.model.push(0.4, Point::new(3000., 1000.));
        let mut source = Effect::new(Params::default(), 2);
        source.set_active(0., true);
        source.push(0.05, Point::new(10., 10.));
        source.push(0.1, Point::new(20., 10.));
        let state = source.continuation(0.1).unwrap();
        input.resume(0.5, &state, None);
        assert!(input.retired.is_empty());
        assert_eq!(input.model.particle_count(), 0);
        let imported = input.model.continuation(0.5).unwrap();
        assert_eq!(imported.points.len(), state.points.len());
        for (got, expected) in imported.points.iter().zip(&state.points) {
            assert_eq!(got.pos, expected.pos);
            assert!((got.age_secs - expected.age_secs).abs() < 1e-12);
        }
        assert_eq!(input.model.params().trail_secs, 10.);
    }

    #[test]
    fn duration_survives_retirement_and_pause_and_updates_existing_departures() {
        let trail = CursorTrail {
            shared: Arc::new(Shared {
                health: diagnostics::Health::new(diagnostics::Kind::Trail),
                input: Mutex::new(Input::default()),
                cv: Condvar::new(),
                origin: Instant::now(),
                stats: Mutex::default(),
            }),
        };
        trail.set_trail_duration(10.);
        {
            let mut input = lock(&trail.shared.input);
            input.model.set_active(0., true);
            input.model.push(0.1, Point::new(1., 0.));
            input.model.depart(0.1, Point::new(1., 0.));
            input.retire(0.1);
            assert_eq!(input.model.params().trail_secs, 10.);
            assert_eq!(input.retired[0].params().trail_secs, 10.);
        }
        trail.set_trail_duration(60.);
        assert_eq!(lock(&trail.shared.input).retired[0].params().trail_secs, 60.);
        trail.set_paused(true);
        assert_eq!(lock(&trail.shared.input).model.params().trail_secs, 60.);
    }

    use super::*;
    use oink_foundation::geom::{Rect, Size};

    fn display(bounds: Rect, px_w: u32, reference_width: Option<f64>) -> DisplayInfo {
        DisplayInfo {
            key: "d".into(),
            name: "d".into(),
            bounds,
            pixel_width: px_w,
            pixel_height: 0,
            reference_extent: reference_width.map(|w| Size::new(w, w * 0.6)),
            rotation_deg: 0,
            is_primary: true,
            is_builtin: true,
            is_virtual: false,
        }
    }

    #[test]
    fn departure_fades_independently_until_arrival_replaces_local_sessions() {
        let mut input = Input::default();
        input.model.set_active(0., true);
        for i in 0..=60 {
            input
                .model
                .push(f64::from(i) / 120., Point::new(f64::from(i) * 3., 100.));
        }
        let mut incoming = input.model.continuation(0.5).unwrap();
        input.model.depart(0.5, Point::new(180., 100.));
        let particles = input.model.particle_count();
        input.retire(0.5);
        assert_eq!(input.retired.len(), 1);
        incoming.size_scale = 0.67;
        assert_eq!(input.retired[0].particle_count(), particles);
        assert_eq!(input.retired[0].size_scale(0.6), 1.);
        assert!(input.retired[0].ribbon_count(0.6) > 0);
        assert_eq!(input.retired[0].envelope(0.8), 1.);
        assert_eq!(input.retired[0].ribbon_count(0.8), 0);
        assert!(input.retired[0].alive(0.8));
        // Regridding for the shared Windows/macOS canvas preserves local sizes.
        let mut copy = input.retired[0].clone();
        let mut before = Vec::new();
        let b = copy.bounds(0.6).unwrap();
        copy.render(0.6, b, &mut before);
        copy.regrid(2.);
        copy.regrid(1.);
        let mut after = Vec::new();
        copy.render(0.6, b, &mut after);
        assert_eq!(before, after);
        input.resume(0.6, &incoming, None);
        assert!(input.retired.is_empty());
        assert!((input.model.size_scale(0.6) - 0.67).abs() < 1e-12);
    }

    #[test]
    fn two_hz_crossings_keep_every_retired_instance_disconnected() {
        for vertical in [false, true] {
            let mut screens = [Input::default(), Input::default()];
            let mut holder = 0;
            screens[0].model.set_active(0., true);
            for i in 0..500 {
                let t = f64::from(i) * 0.008;
                let angle = std::f64::consts::PI + t * std::f64::consts::TAU * 2.;
                let p = Point::new(1000. + 60. * angle.cos(), 400. + 60. * angle.sin());
                let next = usize::from(p.x > 1000.);
                let local = |p: Point, screen: usize| {
                    let p = Point::new(p.x - 1000. * screen as f64, p.y);
                    if vertical { Point::new(p.y, p.x) } else { p }
                };
                if next != holder {
                    let exit = local(Point::new(1000., p.y), holder);
                    screens[holder].model.push(t, exit);
                    let mut state = screens[holder].model.continuation(t).unwrap();
                    screens[holder].model.depart(t, exit);
                    screens[holder].retire(t);
                    let shift = 1000. * (holder as f64 - next as f64);
                    for point in &mut state.points {
                        if vertical {
                            point.pos.y += shift;
                        } else {
                            point.pos.x += shift;
                        }
                    }
                    // The entry notification can arrive before resume is dispatched.
                    // It must go into a fresh model, never the retiring path.
                    screens[next].model.push(t, local(p, next));
                    screens[next].resume(t, &state, None);
                    holder = next;
                }
                screens[holder].model.push(t, local(p, holder));
                for screen in &screens {
                    for e in std::iter::once(&screen.model).chain(screen.retired.iter()) {
                        assert!(
                            e.ribbon_max_step(t) < 25.,
                            "unexpected exit-entry chord: {}",
                            e.ribbon_max_step(t)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn cells_are_whole_device_pixels_at_reference_extent() {
        let close = |(c, plane): (f64, f64), want: f64, want_reference: f64| {
            assert!(
                (c - want).abs() < 1e-9 && (plane - want_reference).abs() < 0.01 / 686.0,
                "{c} {plane}"
            );
        };
        // 16 寸 MacBook：1728 pt、3456 px、宽 345/686 reference units → 0.1/686 reference units/px → 2 px = 1 pt。
        let mac = display(Rect::new(0., 0., 1728., 1117.), 3456, Some(345.0 / 686.0));
        close(cell_for(&mac), 1.0, 0.2 / 686.0);
        // 4K 28 寸（PMv2 物理像素）：620/686 reference units / 3840 px = 0.16 → 1 px。
        let k4 = display(Rect::new(0., 0., 3840., 2160.), 3840, Some(620.0 / 686.0));
        close(cell_for(&k4), 1.0, 0.16 / 686.0);
        // 1080p 24 寸：0.28/686 reference units/px → 至少 1 px。
        let p1080 = display(Rect::new(0., 0., 1920., 1080.), 1920, Some(531.0 / 686.0));
        close(cell_for(&p1080), 1.0, 0.28 / 686.0);
        // Windows 高分屏笔记本（物理像素，13 寸 2880 px / 290/686 reference units = 0.1/686 reference units）：2 px，平台层放大。
        let hidpi = display(Rect::new(0., 0., 2880., 1800.), 2880, Some(290.0 / 686.0));
        close(cell_for(&hidpi), 2.0, 0.2 / 686.0);
        // 不知道物理尺寸：1 个本机单位。
        let unknown = display(Rect::new(0., 0., 1440., 900.), 2880, None);
        close(cell_for(&unknown), 1.0, 0.2 / 686.0);
    }
}
