//! 小游戏的画面（两个平台共用）：把「本机要显示的游戏状态」变成每块屏幕上的一组小图块（sprite）。
//!
//! 背景是不透明的纯色，由平台层的窗口自己填；这里只产生盖在背景上的图块：标靶、点中时的爆炸、计分栏、
//! 倒计时、结果。每个图块有稳定的 `id` 和内容版本 `version`，平台层只在版本变化时更新（macOS：一个图块
//! 一个图层；Windows：按脏矩形重新合成），所以静止的画面没有开销，动画只重画爆炸那一小块。
//!
//! 点击的本机预测也在这里：点击落在某个标靶上时立即开始爆炸动画并把它藏起来，不等权威的应答（权威在
//! 另一台设备上时晚一个网络往返）。权威的状态是唯一的事实：预测错了，标靶在下一条状态里照常出现。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::model::AimPhase;
use oink_foundation::{Point, Rect};

use super::raster::{Canvas, Color, over_px};
use super::text::{self, TextRaster};

/// 叠加层的背景色（不透明，预乘 BGRA）。
pub const BACKGROUND: u32 = 0xFF0C_0E13;

/// HUD height in reference-plane units.
pub const HUD_HEIGHT_RATIO: f64 = 16.0 / 686.0;
/// Text cap heights use the same reference-plane conversion as the targets.
pub const HUD_LABEL_CAP_RATIO: f64 = 2.4 / 686.0;
pub const HUD_VALUE_CAP_RATIO: f64 = 5.6 / 686.0;
/// Hint text keeps a readable minimum at ordinary display densities.
pub const HUD_HINT_CAP_RATIO: f64 = 2.0 / 686.0;
/// Built-in displays reserve extra space for camera cutouts.
const TOP_INSET_BUILTIN_RATIO: f64 = 9.0 / 686.0;
const TOP_INSET_RATIO: f64 = 3.0 / 686.0;

/// Reference-plane height reserved above the target region.
pub fn hud_reserved(builtin: bool) -> f64 {
    top_inset(builtin) + HUD_HEIGHT_RATIO
}

fn top_inset(builtin: bool) -> f64 {
    if builtin {
        TOP_INSET_BUILTIN_RATIO
    } else {
        TOP_INSET_RATIO
    }
}

/// 点中后爆炸动画的时长（秒）。
const EXPLOSION_SECS: f64 = 0.42;
/// 没点中时的小涟漪的时长（秒）。
const MISS_SECS: f64 = 0.25;
/// 本机预测点中后，把标靶藏多久等权威确认（秒）。
const PREDICT_SECS: f64 = 0.6;

const TARGET_LIGHT: Color = Color::rgb(170.0, 246.0, 255.0);
const TARGET_BASE: Color = Color::rgb(34.0, 200.0, 222.0);
const TARGET_DARK: Color = Color::rgb(10.0, 112.0, 140.0);
const TEXT: Color = Color::rgb(236.0, 240.0, 248.0);
const TEXT_DIM: Color = Color::rgb(128.0, 138.0, 158.0);
const ACCENT: Color = Color::rgb(34.0, 200.0, 222.0);
const PANEL: Color = Color::rgb(22.0, 26.0, 36.0);
const MISS: Color = Color::rgb(255.0, 110.0, 110.0);

/// 一块本机屏幕。
#[derive(Clone, Debug, PartialEq)]
pub struct SceneDisplay {
    /// 在本机全局坐标中的范围（`DisplayInfo::bounds`）。
    pub bounds: Rect,
    /// 每个本机单位多少个设备像素（Retina 为 2，Windows 为 1）。
    pub pixels_per_unit: f64,
    /// Native units per reference-plane unit (`local.w / rect.w`).
    pub units_per_ratio: f64,
    pub builtin: bool,
}

impl SceneDisplay {
    pub fn px_per_ratio(&self) -> f64 {
        (self.units_per_ratio * self.pixels_per_unit).max(68.6)
    }

    /// 屏幕的设备像素大小。
    pub fn px_size(&self) -> (u32, u32) {
        (
            (self.bounds.w * self.pixels_per_unit).round().max(1.0) as u32,
            (self.bounds.h * self.pixels_per_unit).round().max(1.0) as u32,
        )
    }
}

/// 本机要显示的游戏状态（权威的状态换算成本机坐标之后）。
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub phase: AimPhase,
    /// 当前阶段还剩多久（交给叠加层的那一刻）。
    pub remaining: Duration,
    pub score: u32,
    pub clicks: u32,
    /// 暂停中开的局（各设备用自己的原生输入）。
    pub native_input: bool,
    pub targets: Vec<ViewTarget>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            phase: AimPhase::Countdown,
            remaining: Duration::ZERO,
            score: 0,
            clicks: 0,
            native_input: false,
            targets: Vec::new(),
        }
    }
}

/// 本机屏幕上的一个标靶：圆心与半径都是本机全局坐标的单位。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewTarget {
    pub id: u32,
    pub center: Point,
    pub radius: f64,
}

/// 一个图块：屏幕内的设备像素坐标（左上原点），预乘 BGRA。
#[derive(Clone, Debug)]
pub struct Sprite {
    pub id: u64,
    pub version: u64,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub pixels: Arc<Vec<u32>>,
}

impl Sprite {
    fn new(id: u64, version: u64, x: f64, y: f64, c: Canvas) -> Self {
        Self {
            id,
            version,
            x: x.round() as i32,
            y: y.round() as i32,
            w: c.w as u32,
            h: c.h as u32,
            pixels: Arc::new(c.px),
        }
    }

    pub fn rect(&self) -> PxRect {
        PxRect {
            x: self.x,
            y: self.y,
            w: self.w as i32,
            h: self.h as i32,
        }
    }
}

/// 设备像素矩形（屏幕内坐标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PxRect {
    pub fn intersect(&self, o: &PxRect) -> Option<PxRect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        (x1 > x0 && y1 > y0).then_some(PxRect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    }
}

const ID_TARGET: u64 = 1 << 32;
const ID_BURST: u64 = 2 << 32;
const ID_HUD: u64 = 3 << 32;
const ID_COUNTDOWN: u64 = 4 << 32;
const ID_RESULT: u64 = 5 << 32;

#[derive(Clone, Copy, Debug)]
struct Burst {
    serial: u64,
    display: usize,
    /// 屏幕内的设备像素。
    cx: f64,
    cy: f64,
    /// 标靶半径（像素）；没点中时是涟漪的大小。
    r: f64,
    hit: bool,
    t0: f64,
}

/// 画面状态。时间 `now` 是调用方的单调秒数。
#[derive(Debug)]
pub struct Scene {
    displays: Vec<SceneDisplay>,
    view: View,
    view_at: f64,
    bursts: Vec<Burst>,
    next_burst: u64,
    frame: u64,
    /// 本机预测已点中、等权威确认的标靶（id、到期时刻）。
    hidden: Vec<(u32, f64)>,
    /// 标靶的图（按半径的像素数缓存，所有同样大小的标靶共用）。
    target_cache: HashMap<u32, Arc<Vec<u32>>>,
    /// 每块屏幕上内容不常变的图块（计分栏、倒计时、结果）：`(id, version)` 相同就复用。
    static_cache: HashMap<(usize, u64), Sprite>,
    /// 文字（系统字体；测试里是线段字形）。
    font: Box<dyn TextRaster>,
}

impl Scene {
    /// 用本机的系统文字。
    pub fn new(displays: Vec<SceneDisplay>) -> Self {
        Self::with_text(displays, text::system())
    }

    pub fn with_text(displays: Vec<SceneDisplay>, font: Box<dyn TextRaster>) -> Self {
        Self {
            font,
            displays,
            view: View::default(),
            view_at: 0.0,
            bursts: Vec::new(),
            next_burst: 0,
            frame: 0,
            hidden: Vec::new(),
            target_cache: HashMap::new(),
            static_cache: HashMap::new(),
        }
    }

    pub fn displays(&self) -> &[SceneDisplay] {
        &self.displays
    }

    pub fn view(&self) -> &View {
        &self.view
    }

    /// 权威的新状态。
    pub fn set_view(&mut self, view: View, now: f64) {
        // 权威已经把预测点中的标靶换掉了：不用再藏。
        self.hidden
            .retain(|(id, until)| now < *until && view.targets.iter().any(|t| t.id == *id));
        self.view = view;
        self.view_at = now;
    }

    /// 当前阶段此刻还剩多少秒。
    pub fn remaining(&self, now: f64) -> f64 {
        (self.view.remaining.as_secs_f64() - (now - self.view_at)).max(0.0)
    }

    /// 是否有动画在进行（需要连续出帧）。
    pub fn animating(&self, now: f64) -> bool {
        self.bursts
            .iter()
            .any(|b| now - b.t0 < if b.hit { EXPLOSION_SECS } else { MISS_SECS })
    }

    /// 本机叠加层收到一次左键点击（本机全局坐标）。进行中时立即给出反馈：落在标靶上 → 爆炸并把它藏起来，
    /// 返回它的 id；否则一个小涟漪。判定与计分仍由权威做。
    pub fn click(&mut self, pos: Point, now: f64) -> Option<u32> {
        if self.view.phase != AimPhase::Running {
            return None;
        }
        let display = self.displays.iter().position(|d| d.bounds.contains(pos))?;
        let d = &self.displays[display];
        let ppu = d.pixels_per_unit;
        let to_px = |p: Point| ((p.x - d.bounds.x) * ppu, (p.y - d.bounds.y) * ppu);
        self.hidden.retain(|(_, until)| now < *until);
        let hit = self.view.targets.iter().find(|t| {
            !self.hidden.iter().any(|(id, _)| *id == t.id)
                && (t.center.x - pos.x).powi(2) + (t.center.y - pos.y).powi(2) <= t.radius * t.radius
        });
        let serial = self.next_burst;
        self.next_burst += 1;
        match hit {
            Some(t) => {
                let (cx, cy) = to_px(t.center);
                self.bursts.push(Burst {
                    serial,
                    display,
                    cx,
                    cy,
                    r: t.radius * ppu,
                    hit: true,
                    t0: now,
                });
                self.hidden.push((t.id, now + PREDICT_SECS));
                Some(t.id)
            }
            None => {
                let (cx, cy) = to_px(pos);
                self.bursts.push(Burst {
                    serial,
                    display,
                    cx,
                    cy,
                    r: d.px_per_ratio() * (2.2 / 686.0),
                    hit: false,
                    t0: now,
                });
                None
            }
        }
    }

    /// 这一帧每块屏幕上的图块（按从下到上的顺序）。
    pub fn frame(&mut self, now: f64) -> Vec<Vec<Sprite>> {
        self.frame += 1;
        self.bursts
            .retain(|b| now - b.t0 < if b.hit { EXPLOSION_SECS } else { MISS_SECS });
        self.hidden.retain(|(_, until)| now < *until);
        let remaining = self.remaining(now);
        let mut out = Vec::with_capacity(self.displays.len());
        let mut live = Vec::new();
        for (index, d) in self.displays.clone().iter().enumerate() {
            let mut sprites = Vec::new();
            let ppu = d.pixels_per_unit;
            if self.view.phase == AimPhase::Running {
                for t in &self.view.targets {
                    if !d.bounds.contains(t.center) || self.hidden.iter().any(|(id, _)| *id == t.id) {
                        continue;
                    }
                    let r = t.radius * ppu;
                    // 半径按 1/4 像素取整后缓存；图的边长固定，位置取整到像素。
                    let key = (r * 4.0).round().max(4.0) as u32;
                    let pixels = self
                        .target_cache
                        .entry(key)
                        .or_insert_with(|| Arc::new(target_canvas(f64::from(key) / 4.0).px))
                        .clone();
                    let side = target_side(f64::from(key) / 4.0);
                    let cx = (t.center.x - d.bounds.x) * ppu;
                    let cy = (t.center.y - d.bounds.y) * ppu;
                    sprites.push(Sprite {
                        id: ID_TARGET | u64::from(t.id),
                        version: u64::from(key),
                        x: (cx - side as f64 / 2.0).round() as i32,
                        y: (cy - side as f64 / 2.0).round() as i32,
                        w: side as u32,
                        h: side as u32,
                        pixels,
                    });
                }
            }
            for b in self.bursts.iter().filter(|b| b.display == index) {
                let c = burst_canvas(b, now);
                let half = c.w as f64 / 2.0;
                sprites.push(Sprite::new(
                    ID_BURST | (b.serial & 0xFFFF_FFFF),
                    self.frame,
                    b.cx - half,
                    b.cy - half,
                    c,
                ));
            }
            let (pw, ph) = d.px_size();
            let m = d.px_per_ratio();
            match self.view.phase {
                AimPhase::Countdown | AimPhase::Running => {
                    let time = if self.view.phase == AimPhase::Running {
                        let s = remaining.ceil() as u64;
                        format!("{}:{:02}", s / 60, s % 60)
                    } else {
                        "-:--".to_string()
                    };
                    let acc = if self.view.clicks > 0 {
                        format!(
                            "{}%",
                            (f64::from(self.view.score) * 100.0 / f64::from(self.view.clicks)).round()
                        )
                    } else {
                        "--".to_string()
                    };
                    let version = hash(&(self.view.score, &acc, &time, self.view.native_input));
                    let native = self.view.native_input;
                    let score = self.view.score;
                    live.push((index, ID_HUD));
                    sprites.push(match self.cached(index, ID_HUD, version) {
                        Some(s) => s,
                        None => {
                            let c = hud_canvas(&*self.font, m, f64::from(pw), score, &acc, &time, native);
                            let x = (f64::from(pw) - c.w as f64) / 2.0;
                            self.cache(index, Sprite::new(ID_HUD, version, x, top_inset(d.builtin) * m, c))
                        }
                    });
                    if self.view.phase == AimPhase::Countdown {
                        let digit = (remaining.ceil() as u64).clamp(1, 9);
                        live.push((index, ID_COUNTDOWN));
                        sprites.push(match self.cached(index, ID_COUNTDOWN, digit) {
                            Some(s) => s,
                            None => {
                                let c = countdown_canvas(&*self.font, m, digit);
                                let x = (f64::from(pw) - c.w as f64) / 2.0;
                                let y = (f64::from(ph) - c.h as f64) / 2.0;
                                self.cache(index, Sprite::new(ID_COUNTDOWN, digit, x, y, c))
                            }
                        });
                    }
                }
                AimPhase::Finished => {
                    let (score, clicks, native) = (self.view.score, self.view.clicks, self.view.native_input);
                    let version = hash(&(score, clicks, native));
                    live.push((index, ID_RESULT));
                    sprites.push(match self.cached(index, ID_RESULT, version) {
                        Some(s) => s,
                        None => {
                            let c = result_canvas(&*self.font, m, f64::from(pw), score, clicks, native);
                            let x = (f64::from(pw) - c.w as f64) / 2.0;
                            let y = (f64::from(ph) - c.h as f64) / 2.0;
                            self.cache(index, Sprite::new(ID_RESULT, version, x, y, c))
                        }
                    });
                }
            }
            out.push(sprites);
        }
        self.static_cache.retain(|k, _| live.contains(k));
        out
    }

    fn cached(&self, display: usize, id: u64, version: u64) -> Option<Sprite> {
        self.static_cache
            .get(&(display, id))
            .filter(|s| s.version == version)
            .cloned()
    }

    fn cache(&mut self, display: usize, sprite: Sprite) -> Sprite {
        self.static_cache.insert((display, sprite.id), sprite.clone());
        sprite
    }
}

fn hash<T: std::hash::Hash>(v: &T) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish() | 1
}

fn target_side(r: f64) -> usize {
    (r.ceil() as usize + 2) * 2
}

/// 一个标靶：恰好半径 `r` 像素的实心圆（看到的范围就是算命中的范围），左上方受光的球面明暗。
fn target_canvas(r: f64) -> Canvas {
    let side = target_side(r);
    let mut c = Canvas::new(side, side);
    let mid = side as f64 / 2.0;
    c.disc(mid, mid, r, |dx, dy, d| {
        // 高光在左上 1/3 处。
        let h = ((dx + 0.32 * r).powi(2) + (dy + 0.36 * r).powi(2)).sqrt() / r;
        let body = if h < 0.55 {
            TARGET_LIGHT.mix(TARGET_BASE, h / 0.55)
        } else {
            TARGET_BASE.mix(TARGET_DARK, (h - 0.55) / 0.95)
        };
        // 边上一圈略亮，在深色背景上轮廓清楚。
        let rim = ((d - (r - r * 0.09)) / (r * 0.09)).clamp(0.0, 1.0);
        body.mix(TARGET_LIGHT, rim * 0.35)
    });
    c
}

fn ease_out(u: f64) -> f64 {
    1.0 - (1.0 - u).powi(3)
}

fn burst_canvas(b: &Burst, now: f64) -> Canvas {
    let r = b.r;
    if !b.hit {
        // 没点中：一个向外扩、很快消失的小红圈。
        let u = ((now - b.t0) / MISS_SECS).clamp(0.0, 1.0);
        let side = (r.ceil() as usize + 3) * 2;
        let mut c = Canvas::new(side, side);
        let mid = side as f64 / 2.0;
        c.ring(
            mid,
            mid,
            r * (0.35 + 0.6 * ease_out(u)),
            (r * 0.22 * (1.0 - u)).max(1.0),
            MISS.alpha((1.0 - u).powf(1.5) * 0.9),
        );
        return c;
    }
    let u = ((now - b.t0) / EXPLOSION_SECS).clamp(0.0, 1.0);
    let half = (r * 3.3).ceil() as usize + 2;
    let mut c = Canvas::new(half * 2, half * 2);
    let mid = half as f64;
    let e = ease_out(u);
    // 闪光：原地一个迅速变淡的亮圆。
    if u < 0.5 {
        let a = (1.0 - u / 0.5).powi(2) * 0.85;
        c.disc(mid, mid, r * (1.0 + 0.3 * u), |_, _, _| TARGET_LIGHT.alpha(a));
    }
    // 冲击波：向外扩的圆环，越扩越细。
    c.ring(
        mid,
        mid,
        r * (1.0 + 1.25 * e),
        (r * 0.26 * (1.0 - u)).max(0.8),
        TARGET_BASE.mix(TARGET_LIGHT, 0.5).alpha((1.0 - u).powf(1.4)),
    );
    // 碎片：12 个小圆向四周飞散、减速、缩小。
    let mut seed = b.serial.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut rand = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    for i in 0..12 {
        let angle = (f64::from(i) + rand() * 0.8) / 12.0 * std::f64::consts::TAU;
        let reach = r * (0.5 + (1.3 + 1.1 * rand()) * e);
        let size = r * (0.11 + 0.1 * rand()) * (1.0 - u * 0.85);
        let color = TARGET_BASE.mix(TARGET_LIGHT, rand()).alpha((1.0 - u).powf(1.2));
        c.disc(mid + angle.cos() * reach, mid + angle.sin() * reach, size, |_, _, _| {
            color
        });
    }
    // 「+1」：从圆心向上飘。
    let cap = r * 0.62;
    c.text_centered(
        mid,
        mid - cap / 2.0 - r * 1.1 * e,
        cap,
        "+1",
        TEXT.alpha((1.0 - u).powf(0.8)),
    );
    c
}

/// 计分栏：三栏（得分、命中率、剩余时间），下面一行说明。窄屏按比例缩小。
fn hud_canvas(font: &dyn TextRaster, m: f64, screen_w: f64, score: u32, acc: &str, time: &str, native: bool) -> Canvas {
    const COLUMN_RATIO: f64 = 46.0 / 686.0;
    let m = m * ((screen_w / m - 8.0 / 686.0) / (COLUMN_RATIO * 3.0)).clamp(0.3, 1.0);
    let col = COLUMN_RATIO * m;
    let (w, h) = ((col * 3.0).ceil(), (HUD_HEIGHT_RATIO * m).ceil());
    let mut c = Canvas::new(w as usize, h as usize);
    let score = score.to_string();
    for (i, (label, value, color)) in [
        ("得分", score.as_str(), ACCENT),
        ("命中率", acc, TEXT),
        ("剩余时间", time, TEXT),
    ]
    .into_iter()
    .enumerate()
    {
        let cx = col * (i as f64 + 0.5);
        text::Pen::new(&mut c, font).centered(cx, (0.9 / 686.0) * m, HUD_LABEL_CAP_RATIO * m, label, false, TEXT_DIM);
        text::Pen::new(&mut c, font).tabular_centered(
            cx,
            (5.3 / 686.0) * m,
            HUD_VALUE_CAP_RATIO * m,
            value,
            true,
            color,
        );
    }
    text::Pen::new(&mut c, font).centered(
        w / 2.0,
        (12.9 / 686.0) * m,
        HUD_HINT_CAP_RATIO * m,
        hint(native),
        false,
        TEXT_DIM,
    );
    c
}

/// 计分栏下面的提示行。用词与界面一致：状态叫「全组已暂停」。
fn hint(native: bool) -> &'static str {
    if native {
        "全组已暂停，每台设备用自己的鼠标 · 按 Esc 结束"
    } else {
        "按 Esc 结束"
    }
}

/// 结果里这一局的方式：与界面「瞄准练习」页的用词一致。
fn mode_label(native: bool) -> &'static str {
    if native {
        "全组暂停（每台用自己的鼠标）"
    } else {
        "共享开启（一套键鼠跨设备）"
    }
}

fn countdown_canvas(font: &dyn TextRaster, m: f64, digit: u64) -> Canvas {
    let (w, h) = (((70.0 / 686.0) * m).ceil(), ((58.0 / 686.0) * m).ceil());
    let mut c = Canvas::new(w as usize, h as usize);
    text::Pen::new(&mut c, font).centered(w / 2.0, (3.0 / 686.0) * m, (4.0 / 686.0) * m, "准备", false, TEXT_DIM);
    text::Pen::new(&mut c, font).centered(
        w / 2.0,
        (15.0 / 686.0) * m,
        (36.0 / 686.0) * m,
        &digit.to_string(),
        true,
        ACCENT,
    );
    c
}

fn result_canvas(font: &dyn TextRaster, m: f64, screen_w: f64, score: u32, clicks: u32, native: bool) -> Canvas {
    const WIDTH_RATIO: f64 = 170.0 / 686.0;
    let m = m * ((screen_w / m - 8.0 / 686.0) / WIDTH_RATIO).clamp(0.3, 1.0);
    let (w, h) = ((WIDTH_RATIO * m).ceil(), ((74.0 / 686.0) * m).ceil());
    let mut c = Canvas::new(w as usize, h as usize);
    c.round_rect(0.5, 0.5, w - 1.0, h - 1.0, (5.0 / 686.0) * m, PANEL);
    let cx = w / 2.0;
    text::Pen::new(&mut c, font).centered(cx, (7.0 / 686.0) * m, (4.2 / 686.0) * m, "时间到", false, TEXT_DIM);
    text::Pen::new(&mut c, font).centered(
        cx,
        (19.0 / 686.0) * m,
        (13.0 / 686.0) * m,
        &format!("得分 {score}"),
        true,
        ACCENT,
    );
    let acc = if clicks > 0 {
        format!(
            "命中率 {}% · {} / {} 次点击",
            (f64::from(score) * 100.0 / f64::from(clicks)).round(),
            score,
            clicks
        )
    } else {
        "命中率 — · 没有点击".to_string()
    };
    // 这一行最长：放不下时缩小字。
    let cap = ((4.0 / 686.0) * m)
        .min((4.0 / 686.0) * m * (w - (12.0 / 686.0) * m) / text::width(font, (4.0 / 686.0) * m, &acc, false).max(1.0));
    text::Pen::new(&mut c, font).centered(cx, (41.0 / 686.0) * m, cap, &acc, false, TEXT);
    text::Pen::new(&mut c, font).centered(
        cx,
        (52.5 / 686.0) * m,
        (2.7 / 686.0) * m,
        mode_label(native),
        false,
        TEXT_DIM,
    );
    text::Pen::new(&mut c, font).centered(
        cx,
        (63.0 / 686.0) * m,
        (2.2 / 686.0) * m,
        "点击或按 Esc 关闭",
        false,
        TEXT_DIM.alpha(0.85),
    );
    c
}

/// 两帧之间需要重画的矩形：消失的、出现的、内容或位置变了的图块各自的范围。
pub fn dirty_rects(prev: &[Sprite], next: &[Sprite]) -> Vec<PxRect> {
    let mut out = Vec::new();
    for p in prev {
        match next.iter().find(|n| n.id == p.id) {
            Some(n) if n.version == p.version && n.rect() == p.rect() => {}
            _ => out.push(p.rect()),
        }
    }
    for n in next {
        match prev.iter().find(|p| p.id == n.id) {
            Some(p) if n.version == p.version && n.rect() == p.rect() => {}
            _ => out.push(n.rect()),
        }
    }
    out
}

/// 把矩形 `rect` 范围内的画面合成出来：背景色上按顺序叠各图块。输出不透明的 BGRA，`rect.w × rect.h`。
pub fn compose(rect: PxRect, sprites: &[Sprite], out: &mut Vec<u32>) {
    out.clear();
    out.resize((rect.w.max(0) * rect.h.max(0)) as usize, BACKGROUND);
    for s in sprites {
        let Some(i) = rect.intersect(&s.rect()) else { continue };
        for y in i.y..i.y + i.h {
            let src = ((y - s.y) * s.w as i32 + (i.x - s.x)) as usize;
            let dst = ((y - rect.y) * rect.w + (i.x - rect.x)) as usize;
            for k in 0..i.w as usize {
                out[dst + k] = over_px(out[dst + k], s.pixels[src + k]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aim::text::StrokeText;

    /// 测试用线段字形：逐像素确定，不依赖本机的系统字体。
    fn scene(displays: Vec<SceneDisplay>) -> Scene {
        Scene::with_text(displays, Box::new(StrokeText))
    }

    fn mac() -> SceneDisplay {
        // 16 寸 Mac：1728×1117 pt，Retina 2×，345 mm 宽 → 5.009 pt/mm。
        SceneDisplay {
            bounds: Rect::new(0., 0., 1728., 1117.),
            pixels_per_unit: 2.0,
            units_per_ratio: 1728.0 / (345.0 / 686.0),
            builtin: true,
        }
    }

    fn win() -> SceneDisplay {
        // 4K 28 寸，放在 Mac 右边的本机坐标里（测试用同一台设备的两块屏）：3840 px / 620 mm。
        SceneDisplay {
            bounds: Rect::new(1728., 0., 3840., 2160.),
            pixels_per_unit: 1.0,
            units_per_ratio: 3840.0 / (620.0 / 686.0),
            builtin: false,
        }
    }

    fn running(targets: Vec<ViewTarget>) -> View {
        View {
            phase: AimPhase::Running,
            remaining: Duration::from_millis(41_300),
            score: 12,
            clicks: 14,
            native_input: false,
            targets,
        }
    }

    fn target(id: u32, d: &SceneDisplay, x: f64, y: f64) -> ViewTarget {
        ViewTarget {
            id,
            center: Point::new(d.bounds.x + x, d.bounds.y + y),
            radius: (7.0 / 686.0) * d.units_per_ratio,
        }
    }

    fn find(sprites: &[Sprite], class: u64) -> Vec<&Sprite> {
        sprites.iter().filter(|s| s.id & !0xFFFF_FFFF == class).collect()
    }

    #[test]
    fn targets_have_the_same_physical_size_on_every_screen() {
        let (a, b) = (mac(), win());
        let mut s = scene(vec![a.clone(), b.clone()]);
        s.set_view(
            running(vec![
                target(1, &a, 400., 500.),
                target(2, &b, 1000., 900.),
                target(3, &b, 3000., 300.),
            ]),
            0.0,
        );
        let f = s.frame(0.0);
        let (ta, tb) = (find(&f[0], ID_TARGET), find(&f[1], ID_TARGET));
        assert_eq!((ta.len(), tb.len()), (1, 2));
        for (d, sprite) in [(&a, ta[0]), (&b, tb[0])] {
            // 不透明像素的面积 = π r²（r = 7 mm），按各屏的像素密度换算回 mm²。
            let area_px: f64 = sprite.pixels.iter().map(|p| f64::from(p >> 24) / 255.0).sum();
            let area_ratio = area_px / d.px_per_ratio().powi(2);
            let want = std::f64::consts::PI * (7.0_f64 / 686.0).powi(2);
            assert!((area_ratio - want).abs() / want < 0.02, "{area_ratio} vs {want}");
        }
        // 两个同样大小的标靶共用一张图。
        assert!(Arc::ptr_eq(&tb[0].pixels, &tb[1].pixels));
        // 图块的中心就是标靶的圆心（屏幕内的设备像素）。
        let t = ta[0];
        assert!((f64::from(t.x) + f64::from(t.w) / 2.0 - 800.0).abs() <= 0.5);
        assert!((f64::from(t.y) + f64::from(t.h) / 2.0 - 1000.0).abs() <= 0.5);
    }

    #[test]
    fn a_still_frame_is_unchanged_and_costs_nothing() {
        let a = mac();
        let mut s = scene(vec![a.clone()]);
        s.set_view(running(vec![target(1, &a, 400., 500.)]), 0.0);
        let f1 = s.frame(0.10);
        let f2 = s.frame(0.11);
        assert!(dirty_rects(&f1[0], &f2[0]).is_empty());
        assert!(!s.animating(0.11));
        // 计分栏的图直接复用。
        let (h1, h2) = (find(&f1[0], ID_HUD)[0], find(&f2[0], ID_HUD)[0]);
        assert!(Arc::ptr_eq(&h1.pixels, &h2.pixels));
        // 过了整秒：只有计分栏变。
        let f3 = s.frame(0.40);
        let d = dirty_rects(&f2[0], &f3[0]);
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|r| *r == h1.rect()));
    }

    #[test]
    fn a_click_on_a_target_explodes_and_hides_it_until_the_authority_answers() {
        let a = mac();
        let mut s = scene(vec![a.clone()]);
        let t = target(7, &a, 400., 500.);
        s.set_view(running(vec![t, target(8, &a, 900., 300.)]), 0.0);
        // 圆内靠边：点中。
        let hit = s.click(Point::new(400. + t.radius - 0.5, 500.), 1.0);
        assert_eq!(hit, Some(7));
        assert!(s.animating(1.0));
        let f = s.frame(1.01);
        assert_eq!(find(&f[0], ID_TARGET).len(), 1, "the hit target is hidden at once");
        let burst = find(&f[0], ID_BURST);
        assert_eq!(burst.len(), 1);
        // 爆炸以标靶圆心为中心。
        assert!((f64::from(burst[0].x) + f64::from(burst[0].w) / 2.0 - 800.0).abs() <= 1.0);
        // 同一个位置再点一次：标靶已经没了，是没点中。
        assert_eq!(s.click(Point::new(400., 500.), 1.02), None);
        // 动画每帧都变。
        let f2 = s.frame(1.05);
        assert!(!dirty_rects(&f[0], &f2[0]).is_empty());
        // 权威换了一个新的：旧的不再出现，新的照常显示。
        s.set_view(
            running(vec![target(8, &a, 900., 300.), target(9, &a, 1200., 800.)]),
            1.06,
        );
        assert_eq!(find(&s.frame(1.07)[0], ID_TARGET).len(), 2);
        // 动画结束后没有残留。
        let f = s.frame(1.0 + EXPLOSION_SECS + 0.01);
        assert!(find(&f[0], ID_BURST).is_empty());
        assert!(!s.animating(1.0 + EXPLOSION_SECS + 0.01));
    }

    #[test]
    fn a_wrong_prediction_is_corrected_by_the_authority() {
        let a = mac();
        let mut s = scene(vec![a.clone()]);
        let t = target(7, &a, 400., 500.);
        s.set_view(running(vec![t]), 0.0);
        assert_eq!(s.click(Point::new(400., 500.), 1.0), Some(7));
        // 权威的状态里它还在（例如时间已到、这次点击没算）：过了等待时间后重新显示。
        s.set_view(running(vec![t]), 1.1);
        assert!(find(&s.frame(1.2)[0], ID_TARGET).is_empty());
        assert_eq!(find(&s.frame(1.0 + PREDICT_SECS + 0.01)[0], ID_TARGET).len(), 1);
    }

    #[test]
    fn a_miss_shows_a_small_ripple_and_clicks_outside_the_game_do_nothing() {
        let a = mac();
        let mut s = scene(vec![a.clone()]);
        s.set_view(running(vec![target(7, &a, 400., 500.)]), 0.0);
        assert_eq!(s.click(Point::new(700., 700.), 1.0), None);
        let f = s.frame(1.01);
        assert_eq!(find(&f[0], ID_TARGET).len(), 1);
        let ripple = find(&f[0], ID_BURST);
        assert_eq!(ripple.len(), 1);
        assert!(ripple[0].w < 80, "small: {}", ripple[0].w);
        assert!(find(&s.frame(1.0 + MISS_SECS + 0.01)[0], ID_BURST).is_empty());
        // 倒计时、结束后：点击没有反馈。
        let mut v = running(vec![]);
        v.phase = AimPhase::Countdown;
        s.set_view(v.clone(), 2.0);
        assert_eq!(s.click(Point::new(400., 500.), 2.1), None);
        assert!(!s.animating(2.1));
        // 屏幕之外的点击。
        s.set_view(running(vec![]), 3.0);
        assert_eq!(s.click(Point::new(-50., 10.), 3.1), None);
        assert!(!s.animating(3.1));
    }

    #[test]
    fn hud_sits_below_the_notch_and_inside_the_reserved_strip() {
        for d in [mac(), win()] {
            let mut s = scene(vec![d.clone()]);
            s.set_view(running(vec![]), 0.0);
            let f = s.frame(0.0);
            let hud = find(&f[0], ID_HUD)[0];
            let m = d.px_per_ratio();
            let top = f64::from(hud.y) / m;
            let bottom = f64::from(hud.y + hud.h as i32) / m;
            assert!(top >= if d.builtin { 8.9 / 686.0 } else { 2.9 / 686.0 }, "{top}");
            assert!(bottom <= hud_reserved(d.builtin) + 0.35 / 686.0, "{bottom}");
            // 水平居中、在屏幕内。
            let (pw, _) = d.px_size();
            assert!(hud.x >= 0 && hud.x + hud.w as i32 <= pw as i32);
            assert!((hud.x * 2 + hud.w as i32 - pw as i32).abs() <= 2);
            assert!(hud.pixels.iter().any(|p| *p != 0));
        }
        assert!((hud_reserved(true) - 25.0 / 686.0).abs() < 1e-15);
        assert!((hud_reserved(false) - 19.0 / 686.0).abs() < 1e-15);
    }

    #[test]
    fn hud_text_has_the_same_ratio_size_on_every_screen() {
        // 计分栏的数值在两种像素密度的屏上：墨迹的高度换回毫米相同（线段字形的墨迹 = cap 高 + 一个笔画宽）。
        let mut heights = Vec::new();
        for d in [mac(), win()] {
            let m = d.px_per_ratio();
            let (pw, _) = d.px_size();
            let c = hud_canvas(&StrokeText, m, f64::from(pw), 88, "88", "0:08", false);
            // 数值那一行（标签之下、提示行之上）有墨迹的行。
            let (from, to) = (((4.0 / 686.0) * m) as usize, ((12.0 / 686.0) * m) as usize);
            let rows: Vec<usize> = (from..to)
                .filter(|y| (0..c.w).any(|x| c.px[y * c.w + x] != 0))
                .collect();
            heights.push((rows.last().unwrap() - rows.first().unwrap() + 1) as f64 / m);
            // 整个计分栏的高度也是同样的毫米数。
            assert!((c.h as f64 / m - HUD_HEIGHT_RATIO).abs() < 0.2 / 686.0);
        }
        assert!((heights[0] - heights[1]).abs() < 0.3 / 686.0, "{heights:?}");
        assert!(
            heights[0] >= HUD_VALUE_CAP_RATIO && heights[0] < HUD_VALUE_CAP_RATIO * 1.25,
            "{heights:?}"
        );
    }

    #[test]
    fn hud_text_follows_score_accuracy_and_time() {
        let a = mac();
        let mut s = scene(vec![a.clone()]);
        s.set_view(running(vec![]), 0.0);
        let v0 = find(&s.frame(0.0)[0], ID_HUD)[0].version;
        // 分数变了。
        let mut v = running(vec![]);
        v.score = 13;
        v.clicks = 15;
        s.set_view(v, 0.0);
        let v1 = find(&s.frame(0.0)[0], ID_HUD)[0].version;
        assert_ne!(v0, v1);
        // 同一秒内不变；跨秒变。
        assert_eq!(find(&s.frame(0.2)[0], ID_HUD)[0].version, v1);
        assert_ne!(find(&s.frame(0.4)[0], ID_HUD)[0].version, v1);
        assert!((s.remaining(0.4) - 40.9).abs() < 1e-9);
        // 权威不再发状态：本机时钟倒数到 0 为止。
        assert_eq!(s.remaining(100.0), 0.0);
    }

    #[test]
    fn countdown_and_result_are_centered_on_every_screen() {
        let (a, b) = (mac(), win());
        let mut s = scene(vec![a.clone(), b.clone()]);
        s.set_view(
            View {
                phase: AimPhase::Countdown,
                remaining: Duration::from_millis(2_400),
                ..View::default()
            },
            0.0,
        );
        let f = s.frame(0.0);
        for (d, sprites) in [(&a, &f[0]), (&b, &f[1])] {
            let c = find(sprites, ID_COUNTDOWN)[0];
            assert_eq!(c.version, 3);
            let (pw, ph) = d.px_size();
            assert!((c.x * 2 + c.w as i32 - pw as i32).abs() <= 2);
            assert!((c.y * 2 + c.h as i32 - ph as i32).abs() <= 2);
            assert!(find(sprites, ID_TARGET).is_empty());
        }
        assert_eq!(find(&s.frame(0.5)[0], ID_COUNTDOWN)[0].version, 2);
        assert_eq!(find(&s.frame(2.39)[0], ID_COUNTDOWN)[0].version, 1);
        let mut v = running(vec![target(1, &a, 100., 400.)]);
        v.phase = AimPhase::Finished;
        v.remaining = Duration::ZERO;
        s.set_view(v, 3.0);
        let f = s.frame(3.0);
        for sprites in &f {
            assert_eq!(find(sprites, ID_RESULT).len(), 1);
            assert!(find(sprites, ID_HUD).is_empty() && find(sprites, ID_TARGET).is_empty());
        }
    }

    #[test]
    fn narrow_screens_scale_the_hud_and_result_down() {
        // 宽 120 mm 的小屏：三栏计分栏（138 mm）和结果面板（170 mm）都缩到屏幕以内。
        let d = SceneDisplay {
            bounds: Rect::new(0., 0., 600., 400.),
            pixels_per_unit: 1.0,
            units_per_ratio: 5.0 * 686.0,
            builtin: false,
        };
        let mut s = scene(vec![d.clone()]);
        s.set_view(running(vec![]), 0.0);
        let f = s.frame(0.0);
        let hud = find(&f[0], ID_HUD)[0];
        assert!(hud.x >= 0 && hud.x + hud.w as i32 <= 600, "{} {}", hud.x, hud.w);
        let mut v = running(vec![]);
        v.phase = AimPhase::Finished;
        s.set_view(v, 1.0);
        let f = s.frame(1.0);
        let r = find(&f[0], ID_RESULT)[0];
        assert!(r.x >= 0 && r.x + r.w as i32 <= 600);
    }

    #[test]
    fn composing_dirty_rects_equals_composing_the_whole_screen() {
        let a = mac();
        let mut s = scene(vec![a.clone()]);
        let t = target(7, &a, 400., 500.);
        s.set_view(running(vec![t, target(8, &a, 430., 520.)]), 0.0);
        let (pw, ph) = a.px_size();
        let full = PxRect {
            x: 0,
            y: 0,
            w: pw as i32,
            h: ph as i32,
        };
        let f1 = s.frame(0.0);
        let mut screen = Vec::new();
        compose(full, &f1[0], &mut screen);
        assert!(screen.iter().all(|p| p >> 24 == 255), "opaque");
        assert!(screen.iter().any(|p| *p != BACKGROUND));
        // 点中一个（爆炸与旁边的标靶重叠），逐帧只重画脏矩形，结果应与整屏重新合成一致。
        s.click(t.center, 0.5);
        let mut prev = f1;
        let mut buf = Vec::new();
        for i in 1..=30 {
            let now = 0.5 + f64::from(i) * 0.02;
            let next = s.frame(now);
            for r in dirty_rects(&prev[0], &next[0]) {
                let Some(r) = r.intersect(&full) else { continue };
                compose(r, &next[0], &mut buf);
                for y in 0..r.h {
                    let dst = ((r.y + y) * full.w + r.x) as usize;
                    let src = (y * r.w) as usize;
                    screen[dst..dst + r.w as usize].copy_from_slice(&buf[src..src + r.w as usize]);
                }
            }
            let mut want = Vec::new();
            compose(full, &next[0], &mut want);
            assert!(screen == want, "frame {i} differs");
            prev = next;
        }
    }
}
