//! Authoritative game rules: time, ratio-plane regions, and clicks enter a pure state machine.

use super::model::{AimPhase, AimState, AimTarget, GameInstance};
use oink_foundation::{DeviceId, Point, Rect};

/// 微秒（与路由、服务进程的单调时钟同一单位）。
pub type Micros = u64;

/// Target diameter in the shared reference-ratio plane.
pub const TARGET_DIAMETER: f64 = 14.0 / 686.0;
/// 全场同时存在的标靶数。
pub const TARGET_COUNT: usize = 3;
/// 一局的时长。
pub const GAME_MICROS: Micros = 60_000_000;
/// 开局倒计时。
pub const COUNTDOWN_MICROS: Micros = 3_000_000;
/// 两个标靶圆心的最小距离是半径的多少倍（2 = 相切；留出半个半径的空隙）。
pub const SPACING_RADII: f64 = 2.5;
/// 结束后这么久之内的点击不算「关闭结果」（连点时不会误关）。
pub const DISMISS_GUARD_MICROS: Micros = 700_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    pub targets: usize,
    pub radius: f64,
    pub countdown: Micros,
    pub duration: Micros,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            targets: TARGET_COUNT,
            radius: TARGET_DIAMETER / 2.0,
            countdown: COUNTDOWN_MICROS,
            duration: GAME_MICROS,
        }
    }
}

/// 一块屏幕上可以放标靶圆心的范围（视平面毫米；已经从屏幕边缘缩进一个半径、避开顶部的计分栏）。
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub device: DeviceId,
    pub display: String,
    pub rect: Rect,
}

impl Region {
    fn area(&self) -> f64 {
        self.rect.w.max(0.0) * self.rect.h.max(0.0)
    }

    fn holds(&self, t: &AimTarget) -> bool {
        const EPS: f64 = 1e-6 / 686.0;
        self.device == t.device
            && self.display == t.display
            && t.pos.x >= self.rect.x - EPS
            && t.pos.x <= self.rect.right() + EPS
            && t.pos.y >= self.rect.y - EPS
            && t.pos.y <= self.rect.bottom() + EPS
    }
}

/// 一次点击的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Click {
    /// 倒计时中、结束后的保护时间内：不计。
    Ignored,
    Miss,
    Hit {
        id: u32,
    },
    /// 结束后的点击：关闭结果。
    Dismiss,
}

/// 一局的统计。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub score: u32,
    pub clicks: u32,
    /// 实际进行了多久（提前结束时小于一局的时长）。
    pub played: Micros,
    /// 是否打满了时间。
    pub completed: bool,
}

impl Summary {
    /// 命中率（0–1）；没有点击时为 `None`。
    pub fn accuracy(&self) -> Option<f64> {
        (self.clicks > 0).then(|| f64::from(self.score) / f64::from(self.clicks))
    }

    /// 每秒命中数。
    pub fn hits_per_sec(&self) -> f64 {
        if self.played == 0 {
            0.0
        } else {
            f64::from(self.score) / (self.played as f64 / 1e6)
        }
    }

    /// 平均命中间隔（毫秒）；没有命中时为 `None`。
    pub fn avg_hit_interval_ms(&self) -> Option<f64> {
        (self.score > 0).then(|| self.played as f64 / 1e3 / f64::from(self.score))
    }
}

#[derive(Clone, Debug)]
pub struct Game {
    cfg: Config,
    rng: u64,
    phase: AimPhase,
    /// 当前阶段的结束时刻（`Finished`：结束的时刻）。
    until: Micros,
    started: Option<Micros>,
    regions: Vec<Region>,
    targets: Vec<AimTarget>,
    next_id: u32,
    score: u32,
    clicks: u32,
    played: Micros,
    completed: bool,
}

impl Game {
    /// 新的一局，从倒计时开始。`seed` 决定标靶位置的序列。
    pub fn new(cfg: Config, regions: Vec<Region>, now: Micros, seed: u64) -> Self {
        Self {
            cfg,
            rng: seed | 1,
            phase: AimPhase::Countdown,
            until: now + cfg.countdown,
            started: None,
            regions,
            targets: Vec::new(),
            next_id: 1,
            score: 0,
            clicks: 0,
            played: 0,
            completed: false,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn phase(&self) -> AimPhase {
        self.phase
    }

    pub fn score(&self) -> u32 {
        self.score
    }

    pub fn clicks(&self) -> u32 {
        self.clicks
    }

    pub fn targets(&self) -> &[AimTarget] {
        &self.targets
    }

    /// 当前阶段还剩多久（结束后为 0）。
    pub fn remaining(&self, now: Micros) -> Micros {
        match self.phase {
            AimPhase::Finished => 0,
            _ => self.until.saturating_sub(now),
        }
    }

    /// 下一次需要 `tick` 的时刻（阶段切换）；结束后没有。
    pub fn deadline(&self) -> Option<Micros> {
        (self.phase != AimPhase::Finished).then_some(self.until)
    }

    /// 结束的时刻（结束后才有）。
    pub fn finished_at(&self) -> Option<Micros> {
        (self.phase == AimPhase::Finished).then_some(self.until)
    }

    /// 可放标靶的区域变了（设备加入 / 离开、显示器变化）。进行中时，不在任何区域里的标靶立即换到别处。
    /// 返回标靶是否有变化。
    pub fn set_regions(&mut self, regions: Vec<Region>) -> bool {
        self.regions = regions;
        if self.phase != AimPhase::Running {
            return false;
        }
        let before = self.targets.len();
        let regions = &self.regions;
        self.targets.retain(|t| regions.iter().any(|r| r.holds(t)));
        let removed = before - self.targets.len();
        self.fill(None);
        removed > 0 || self.targets.len() != before
    }

    /// 推进阶段。返回状态是否有变化。
    pub fn tick(&mut self, now: Micros) -> bool {
        let mut changed = false;
        if self.phase == AimPhase::Countdown && now >= self.until {
            // 从倒计时结束的时刻起算，不受 tick 晚到的影响。
            let start = self.until;
            self.phase = AimPhase::Running;
            self.started = Some(start);
            self.until = start + self.cfg.duration;
            self.fill(None);
            changed = true;
        }
        if self.phase == AimPhase::Running && now >= self.until {
            self.finish(self.until, true);
            changed = true;
        }
        changed
    }

    /// 提前结束（用户按了 Esc 等）。已经结束时不变。
    pub fn abort(&mut self, now: Micros) {
        if self.phase != AimPhase::Finished {
            self.finish(now, false);
        }
    }

    fn finish(&mut self, at: Micros, completed: bool) {
        self.played = self.started.map_or(0, |s| at.saturating_sub(s)).min(self.cfg.duration);
        self.completed = completed;
        self.phase = AimPhase::Finished;
        self.until = at;
        self.targets.clear();
    }

    /// 一次左键点击（落在叠加层上）。`pos` 是视平面毫米，`device` / `display` 是点击所在的屏幕。
    pub fn click(&mut self, device: DeviceId, display: &str, pos: Point, now: Micros) -> Click {
        self.tick(now);
        match self.phase {
            AimPhase::Countdown => Click::Ignored,
            AimPhase::Finished => {
                if now >= self.until + DISMISS_GUARD_MICROS {
                    Click::Dismiss
                } else {
                    Click::Ignored
                }
            }
            AimPhase::Running => {
                self.clicks += 1;
                let r = self.cfg.radius;
                let hit = self.targets.iter().position(|t| {
                    t.device == device && t.display == display && {
                        let (dx, dy) = (t.pos.x - pos.x, t.pos.y - pos.y);
                        dx * dx + dy * dy <= r * r
                    }
                });
                match hit {
                    Some(i) => {
                        let old = self.targets.remove(i);
                        self.score += 1;
                        // 新标靶也不压在刚被点中的那个位置上（光标还在那里）。
                        self.fill(Some(old.pos));
                        Click::Hit { id: old.id }
                    }
                    None => Click::Miss,
                }
            }
        }
    }

    pub fn summary(&self, now: Micros) -> Summary {
        let played = match self.phase {
            AimPhase::Countdown => 0,
            AimPhase::Running => self.started.map_or(0, |s| now.saturating_sub(s)).min(self.cfg.duration),
            AimPhase::Finished => self.played,
        };
        Summary {
            score: self.score,
            clicks: self.clicks,
            played,
            completed: self.completed,
        }
    }

    /// 发给参与设备 / 交给叠加层的完整状态。
    pub fn state(&self, game: GameInstance, seq: u64, now: Micros, native_input: bool) -> AimState {
        AimState {
            game,
            seq,
            phase: self.phase,
            remaining_ms: (self.remaining(now) / 1000) as u32,
            score: self.score,
            clicks: self.clicks,
            radius: self.cfg.radius as f32,
            native_input,
            targets: self.targets.clone(),
        }
    }

    fn fill(&mut self, avoid: Option<Point>) {
        while self.targets.len() < self.cfg.targets {
            let Some(t) = self.spawn(avoid) else { break };
            self.targets.push(t);
        }
    }

    /// 在所有区域里按面积均匀取一个位置，与现有标靶（和 `avoid`）保持最小间距。试若干次都放不下时
    /// （屏幕太小）取离得最远的那一次。没有区域时返回 `None`。
    fn spawn(&mut self, avoid: Option<Point>) -> Option<AimTarget> {
        let total: f64 = self.regions.iter().map(Region::area).sum();
        if total <= 0.0 {
            return None;
        }
        let spacing = self.cfg.radius * SPACING_RADII;
        let mut best: Option<(f64, usize, Point)> = None;
        for _ in 0..96 {
            let mut pick = self.unit() * total;
            let mut index = self.regions.len() - 1;
            for (i, r) in self.regions.iter().enumerate() {
                if pick < r.area() {
                    index = i;
                    break;
                }
                pick -= r.area();
            }
            let rect = self.regions[index].rect;
            let p = Point::new(rect.x + self.unit() * rect.w, rect.y + self.unit() * rect.h);
            let clearance = self
                .targets
                .iter()
                .map(|t| t.pos)
                .chain(avoid)
                .map(|q| ((q.x - p.x).powi(2) + (q.y - p.y).powi(2)).sqrt())
                .fold(f64::INFINITY, f64::min);
            if best.as_ref().is_none_or(|b| clearance > b.0) {
                best = Some((clearance, index, p));
            }
            if clearance >= spacing {
                break;
            }
        }
        let (_, index, pos) = best?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let r = &self.regions[index];
        Some(AimTarget {
            id,
            device: r.device,
            display: r.display.clone(),
            pos,
        })
    }

    /// `[0, 1)` 的均匀随机数（splitmix64）。
    fn unit(&mut self) -> f64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane_rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect::new(x / 686.0, y / 686.0, w / 686.0, h / 686.0)
    }

    const A: DeviceId = DeviceId([1; 32]);
    const B: DeviceId = DeviceId([2; 32]);

    fn regions() -> Vec<Region> {
        vec![
            // a：300×180 mm；b：面积是 a 的 3 倍。
            Region {
                device: A,
                display: "a".into(),
                rect: plane_rect(7., 30., 300., 180.),
            },
            Region {
                device: B,
                display: "b".into(),
                rect: plane_rect(400., 0., 540., 300.),
            },
        ]
    }

    fn running(seed: u64) -> Game {
        let mut g = Game::new(Config::default(), regions(), 0, seed);
        assert!(g.tick(COUNTDOWN_MICROS));
        assert_eq!(g.phase(), AimPhase::Running);
        g
    }

    fn min_distance(g: &Game) -> f64 {
        let t = g.targets();
        let mut d = f64::INFINITY;
        for i in 0..t.len() {
            for j in i + 1..t.len() {
                d = d.min(((t[i].pos.x - t[j].pos.x).powi(2) + (t[i].pos.y - t[j].pos.y).powi(2)).sqrt());
            }
        }
        d
    }

    #[test]
    fn countdown_has_no_targets_and_ignores_clicks() {
        let mut g = Game::new(Config::default(), regions(), 1_000, 7);
        assert_eq!(g.phase(), AimPhase::Countdown);
        assert!(g.targets().is_empty());
        assert_eq!(g.remaining(1_000), COUNTDOWN_MICROS);
        assert_eq!(
            g.click(A, "a", Point::new(50. / 686.0, 50. / 686.0), 500_000),
            Click::Ignored
        );
        assert_eq!(g.clicks(), 0);
        assert!(!g.tick(COUNTDOWN_MICROS));
        assert!(g.tick(COUNTDOWN_MICROS + 1_000));
        assert_eq!(g.targets().len(), TARGET_COUNT);
        // 一局从倒计时结束的时刻起算，不受 tick 晚到的影响。
        assert_eq!(g.remaining(COUNTDOWN_MICROS + 1_000), GAME_MICROS);
    }

    #[test]
    fn a_hit_scores_and_is_replaced_at_once_without_overlap() {
        let mut g = running(42);
        let mut now = COUNTDOWN_MICROS;
        for n in 1..=400u32 {
            now += 100_000;
            let t = g.targets()[(n % 3) as usize].clone();
            // 圆内靠边的位置也算命中。
            let p = Point::new(t.pos.x + 6.9 / 686.0, t.pos.y);
            assert_eq!(g.click(t.device, &t.display, p, now), Click::Hit { id: t.id });
            assert_eq!(g.score(), n);
            assert_eq!(g.targets().len(), TARGET_COUNT);
            assert!(g.targets().iter().all(|x| x.id != t.id), "replaced");
            assert!(min_distance(&g) >= 7.0 / 686.0 * SPACING_RADII, "{}", min_distance(&g));
            // 新标靶不在刚点中的位置（光标还在那里）。
            let fresh = g.targets().last().unwrap();
            let d = ((fresh.pos.x - t.pos.x).powi(2) + (fresh.pos.y - t.pos.y).powi(2)).sqrt();
            assert!(d >= 7.0 / 686.0 * SPACING_RADII);
            // 都在各自的区域里。
            let rs = regions();
            assert!(g.targets().iter().all(|t| rs.iter().any(|r| r.holds(t))));
            if now + 200_000 >= COUNTDOWN_MICROS + GAME_MICROS {
                break;
            }
        }
    }

    #[test]
    fn a_miss_counts_a_click_but_no_score() {
        let mut g = running(3);
        let t = g.targets()[0].clone();
        let now = COUNTDOWN_MICROS + 10;
        // 圆外一点点。
        assert_eq!(
            g.click(t.device, &t.display, Point::new(t.pos.x + 7.01 / 686.0, t.pos.y), now),
            Click::Miss
        );
        // 位置对、屏幕不对：不算命中（另一块屏幕的坐标）。
        let other = if t.device == A { (B, "b") } else { (A, "a") };
        assert_eq!(g.click(other.0, other.1, t.pos, now), Click::Miss);
        assert_eq!((g.score(), g.clicks()), (0, 2));
        assert_eq!(g.click(t.device, &t.display, t.pos, now), Click::Hit { id: t.id });
        let s = g.summary(now);
        assert_eq!((s.score, s.clicks), (1, 3));
        assert!((s.accuracy().unwrap() - 1.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn positions_are_uniform_by_area_across_screens() {
        let mut g = running(2026);
        let mut on_b = 0u32;
        let mut quadrants = [0u32; 4];
        let n = 6000u32;
        let mut now = COUNTDOWN_MICROS;
        let mut total = 0u32;
        'outer: loop {
            // 重新开局继续采样（一局只有 60 秒）。
            for _ in 0..500 {
                now += 1_000;
                let t = g.targets()[0].clone();
                g.click(t.device, &t.display, t.pos, now);
                let fresh = g.targets().last().unwrap().clone();
                if fresh.device == B {
                    on_b += 1;
                    let qx = usize::from(fresh.pos.x >= (400. + 270.) / 686.0);
                    let qy = usize::from(fresh.pos.y >= 150. / 686.0);
                    quadrants[qy * 2 + qx] += 1;
                }
                total += 1;
                if total == n {
                    break 'outer;
                }
            }
            g = running(u64::from(total));
            now = COUNTDOWN_MICROS;
        }
        // b 的面积占 3/4。
        let share = f64::from(on_b) / f64::from(n);
        assert!((share - 0.75).abs() < 0.03, "{share}");
        for q in quadrants {
            let s = f64::from(q) / f64::from(on_b);
            assert!((s - 0.25).abs() < 0.03, "{quadrants:?}");
        }
    }

    #[test]
    fn time_up_finishes_and_later_clicks_do_not_count() {
        let mut g = running(5);
        let t = g.targets()[0].clone();
        let end = COUNTDOWN_MICROS + GAME_MICROS;
        assert_eq!(g.click(t.device, &t.display, t.pos, end - 1), Click::Hit { id: t.id });
        // 时间到之后的点击不计（即使还没 tick）。
        let t = g.targets()[0].clone();
        assert_eq!(g.click(t.device, &t.display, t.pos, end), Click::Ignored);
        assert_eq!(g.phase(), AimPhase::Finished);
        assert!(g.targets().is_empty());
        assert_eq!(g.remaining(end + 5), 0);
        let s = g.summary(end + 5);
        assert_eq!((s.score, s.clicks, s.played, s.completed), (1, 1, GAME_MICROS, true));
        assert!((s.hits_per_sec() - 1.0 / 60.0).abs() < 1e-9);
        assert_eq!(s.avg_hit_interval_ms(), Some(60_000.0));
        // 保护时间过后，点击关闭结果。
        assert_eq!(
            g.click(A, "a", Point::new(0., 0.), end + DISMISS_GUARD_MICROS - 1),
            Click::Ignored
        );
        assert_eq!(
            g.click(A, "a", Point::new(0., 0.), end + DISMISS_GUARD_MICROS),
            Click::Dismiss
        );
    }

    #[test]
    fn abort_keeps_the_partial_result() {
        let mut g = running(5);
        let t = g.targets()[0].clone();
        g.click(t.device, &t.display, t.pos, COUNTDOWN_MICROS + 1_000_000);
        g.abort(COUNTDOWN_MICROS + 12_000_000);
        let s = g.summary(COUNTDOWN_MICROS + 30_000_000);
        assert_eq!((s.score, s.played, s.completed), (1, 12_000_000, false));
        assert!(!g.tick(COUNTDOWN_MICROS + GAME_MICROS + 1));
        // 倒计时里就结束：没有进行过。
        let mut g = Game::new(Config::default(), regions(), 0, 1);
        g.abort(1_000_000);
        assert_eq!(g.summary(2_000_000).played, 0);
        assert_eq!(g.summary(2_000_000).accuracy(), None);
    }

    #[test]
    fn losing_a_device_moves_its_targets_elsewhere() {
        let mut g = running(11);
        // 让 b 上至少有一个标靶（面积 3/4，三个都不在 b 上的概率很小；换种子直到有）。
        let mut seed = 11;
        while !g.targets().iter().any(|t| t.device == B) {
            seed += 1;
            g = running(seed);
        }
        let only_a = vec![regions().remove(0)];
        assert!(g.set_regions(only_a.clone()));
        assert_eq!(g.targets().len(), TARGET_COUNT);
        assert!(g.targets().iter().all(|t| t.device == A && only_a[0].holds(t)));
        // 没变化时不报告变化。
        assert!(!g.set_regions(only_a));
        // 一块屏幕都没有：没有标靶，也不崩。
        assert!(g.set_regions(Vec::new()));
        assert!(g.targets().is_empty());
    }

    #[test]
    fn a_tiny_screen_still_gets_all_targets() {
        // 放不下最小间距时取离得最远的位置，仍然凑满三个。
        let tiny = vec![Region {
            device: A,
            display: "a".into(),
            rect: plane_rect(0., 0., 20., 10.),
        }];
        let mut g = Game::new(Config::default(), tiny, 0, 9);
        g.tick(COUNTDOWN_MICROS);
        assert_eq!(g.targets().len(), TARGET_COUNT);
    }

    #[test]
    fn the_state_carries_everything_a_participant_renders() {
        let mut g = running(1);
        let t = g.targets()[0].clone();
        g.click(t.device, &t.display, t.pos, COUNTDOWN_MICROS + 2_500_000);
        let game = GameInstance {
            authority_boot: oink_foundation::session::Boot {
                device_id: DeviceId([1; 32]),
                nonce: [1; 16],
            },
            game_counter: 77,
        };
        let s = g.state(game, 3, COUNTDOWN_MICROS + 2_500_000, true);
        assert!(s.valid());
        assert_eq!(
            (s.game, s.seq, s.phase, s.score, s.clicks),
            (game, 3, AimPhase::Running, 1, 1)
        );
        assert_eq!(s.remaining_ms, 57_500);
        assert_eq!(s.radius, (7.0 / 686.0) as f32);
        assert_eq!(s.targets, g.targets());
        assert!(s.native_input);
    }
}
