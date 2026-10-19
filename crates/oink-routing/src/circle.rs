//! Detect a quick circle gesture to reveal the cursor locator trail.
//!
//! 纯状态机：输入是**输入源**（物理键鼠所在设备）看到的连续手部位移（视平面 plane）与单调时间（微秒），
//! 输出是否「已触发」。与指针在哪台设备、哪块屏幕无关：画圈途中指针跨到另一台设备，圈数照常累计。
//!
//! 做法：按路程重采样（每累计 [`CircleParams::step`] 取一次运动方向，避免整数位移的方向量化噪声），
//! 累计**有符号转角**；每 360° 算一圈，从第 [`CircleParams::trigger_turns`] 圈之后（默认 2，即进入
//! 第三圈）触发。触发后只检查停顿，任意非零位移都保持显示；停止后清零，再次显示需重新画圈。
//! 一段连续的「画圈」叫一次 run，触发前下列任一情况清零：
//!
//! - 停顿：超过 `max_gap_ms` 没有位移；
//! - 太慢：最近一整圈用时超过 `1 / 最低角速度`（第一圈未满时：自 run 开始已超过这个时间），或最近
//!   `end_window_ms` 内转过的角度不到按最低角速度应转角度的 `end_min_fraction`（变成直线 / 停住转动）。
//!   最低角速度为 `min_turn_rate`；
//! - 方向不一致：单步转角超过 `max_step_turn_deg`（来回晃动的折返点），或反方向累计转角超过
//!   `max_counter_turn_deg`（反向画圈、8 字）；
//! - 圈的大小不合理：最近一整圈的路程折算半径 `L / 2π` 不在 `[min_radius, max_radius]`，或第一圈
//!   未满时路程已超过 `2π × max_radius`。
//!
//! 「画得够快」只看角速度（运动方向的转动速度），与屏幕尺寸、指针速度设置无关。

use std::collections::VecDeque;
use std::f64::consts::{PI, TAU};

use serde::{Deserialize, Serialize};

/// Circle recognition thresholds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CircleParams {
    /// 重采样步长（视平面 plane）：位移累计到这么长才取一次运动方向。
    pub step: f64,
    /// 最低角速度（圈 / 秒）：未触发时（攒圈数）的要求。
    pub min_turn_rate: f64,
    /// 圈的半径范围（视平面 plane，由一整圈的路程折算）。
    pub min_radius: f64,
    pub max_radius: f64,
    /// 累计满这么多圈后触发（2 = 进入第三圈时）。
    pub trigger_turns: f64,
    /// 单步转角上限（度）：超过视为折返（来回晃动）。
    pub max_step_turn_deg: f64,
    /// 反方向累计转角上限（度）：超过视为方向反转。同方向的转动会抵消它。
    pub max_counter_turn_deg: f64,
    /// 停顿多久（毫秒）没有位移就结束。
    pub max_gap_ms: f64,
    /// 结束判定的短窗口（毫秒）与最低角度比例（相对当前的最低角速度）。
    pub end_window_ms: f64,
    pub end_min_fraction: f64,
}

impl Default for CircleParams {
    fn default() -> Self {
        Self {
            step: 0.8 / 686.0,
            min_turn_rate: 1.5,
            min_radius: 2.5 / 686.0,
            max_radius: 150.0 / 686.0,
            trigger_turns: 2.0,
            max_step_turn_deg: 110.0,
            max_counter_turn_deg: 60.0,
            max_gap_ms: 120.0,
            end_window_ms: 250.0,
            end_min_fraction: 0.5,
        }
    }
}

/// 一次画圈（run）结束、圈数清零的原因（日志与测试用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEnd {
    /// 超过 `max_gap_ms` 没有位移。
    Pause,
    /// 单步转角超过 `max_step_turn_deg`（来回晃动的折返点）。
    SharpTurn,
    /// 反方向累计转角超过 `max_counter_turn_deg`（反向画圈、8 字）。
    CounterTurn,
    /// 最近一整圈（第一圈未满时：自 run 开始）用时超过 `1 / 最低角速度`。
    SlowTurn,
    /// 最近 `end_window_ms` 内转过的角度不够（变成直线 / 停住转动）。
    Stalled,
    /// 最近一整圈折算半径小于 `min_radius`。
    TooSmall,
    /// 最近一整圈折算半径大于 `max_radius`（第一圈未满时：路程已超过一整圈的上限）。
    TooLarge,
    /// 外部清零：设置关闭、游戏模式、按钮按下。
    External,
}

/// 一个重采样点：时间、累计转角（弧度，有符号）、累计路程（plane）。
#[derive(Clone, Copy, Debug)]
struct Step {
    t: u64,
    turn: f64,
    path: f64,
}

/// 画圈识别器。见模块说明。
#[derive(Clone, Debug)]
pub struct CircleDetector {
    p: CircleParams,
    /// 尚未凑满一步的位移。
    acc: (f64, f64),
    last_heading: Option<f64>,
    /// 最近一次有位移的时间。
    last_motion: Option<u64>,
    /// 本次 run 的重采样点（第一个是 run 的起点）；只保留判定需要的最近一段。
    steps: VecDeque<Step>,
    turn: f64,
    path: f64,
    /// 本次 run 的转动方向（+1 / -1），转过 45° 后确定；0 = 未定。
    dir: f64,
    counter: f64,
    engaged: bool,
    /// 最近一次清零的原因。
    last_end: Option<RunEnd>,
}

impl Default for CircleDetector {
    fn default() -> Self {
        Self::new(CircleParams::default())
    }
}

impl CircleDetector {
    pub fn new(p: CircleParams) -> Self {
        Self {
            p,
            acc: (0.0, 0.0),
            last_heading: None,
            last_motion: None,
            steps: VecDeque::new(),
            turn: 0.0,
            path: 0.0,
            dir: 0.0,
            counter: 0.0,
            engaged: false,
            last_end: None,
        }
    }

    pub fn params(&self) -> &CircleParams {
        &self.p
    }

    /// 是否已触发（保持到停止或外部清零）。
    pub fn engaged(&self) -> bool {
        self.engaged
    }

    /// 本次 run 已累计的圈数（不分方向，触发后冻结）。
    pub fn turns(&self) -> f64 {
        self.turn.abs() / TAU
    }

    /// 最近一次清零的原因（诊断、测试用）。
    pub fn last_end(&self) -> Option<RunEnd> {
        self.last_end
    }

    /// 全部清零（按钮按下、游戏模式、关闭设置等）。
    pub fn reset(&mut self) {
        self.clear(RunEnd::External);
    }

    fn clear(&mut self, why: RunEnd) {
        self.log_end(why);
        *self = Self {
            last_end: Some(why),
            ..Self::new(self.p)
        };
    }

    /// 已触发的 run 结束时记 debug 日志（说明拖尾为什么停）；未触发的 run 结束很频繁（普通移动），只记 trace。
    fn log_end(&self, why: RunEnd) {
        if self.engaged {
            tracing::debug!(reason = ?why, turns = self.turns(), "circle gesture ended");
        } else if self.turns() >= 1.0 {
            tracing::trace!(reason = ?why, turns = self.turns(), "circle run ended");
        }
    }

    /// 需要调用 [`CircleDetector::tick`] 的时间（停顿判定）；没有进行中的 run 时为 `None`。
    pub fn deadline(&self) -> Option<u64> {
        let t = self.last_motion?;
        (self.engaged || self.turn != 0.0 || !self.steps.is_empty()).then(|| t + self.gap_us() + 1)
    }

    /// 定时检查停顿。返回是否仍在触发状态。
    pub fn tick(&mut self, now: u64) -> bool {
        if self.last_motion.is_some_and(|t| now > t + self.gap_us()) {
            self.clear(RunEnd::Pause);
        }
        self.engaged
    }

    /// 一次位移（视平面 plane，屏幕坐标方向：x 向右、y 向下）。返回是否处于触发状态。
    pub fn motion(&mut self, dx: f64, dy: f64, now: u64) -> bool {
        if !(dx.is_finite() && dy.is_finite()) {
            return self.engaged;
        }
        if self.last_motion.is_some_and(|t| now > t + self.gap_us()) {
            self.clear(RunEnd::Pause);
        }
        if dx == 0.0 && dy == 0.0 {
            return self.engaged;
        }
        self.last_motion = Some(now);
        // 显示已锁存：速度、方向、形状与重采样步长都不再参与保持判定。
        if self.engaged {
            return true;
        }
        self.acc.0 += dx;
        self.acc.1 += dy;
        let len = self.acc.0.hypot(self.acc.1);
        if len < self.p.step.max(1e-6 / 686.0) {
            return self.engaged;
        }
        let heading = self.acc.1.atan2(self.acc.0);
        self.acc = (0.0, 0.0);
        let Some(prev) = self.last_heading.replace(heading) else {
            self.start(now);
            return self.engaged;
        };
        let d = wrap(heading - prev);
        if d.abs() > self.p.max_step_turn_deg.to_radians() {
            // 折返：来回晃动。从这一步重新开始。
            self.restart(now, RunEnd::SharpTurn);
            return self.engaged;
        }
        if self.steps.is_empty() {
            self.start(now);
        }
        if self.dir != 0.0 && d * self.dir < 0.0 {
            self.counter += d.abs();
            if self.counter > self.p.max_counter_turn_deg.to_radians() {
                self.restart(now, RunEnd::CounterTurn);
                return self.engaged;
            }
        } else {
            self.counter = (self.counter - d.abs()).max(0.0);
        }
        self.turn += d;
        self.path += len;
        if self.dir == 0.0 && self.turn.abs() >= PI / 4.0 {
            self.dir = self.turn.signum();
        }
        self.steps.push_back(Step {
            t: now,
            turn: self.turn,
            path: self.path,
        });
        if let Err(why) = self.consistent(now) {
            self.restart(now, why);
            return self.engaged;
        }
        self.trim();
        if self.turns() >= self.p.trigger_turns {
            tracing::debug!(turns = self.turns(), "circle gesture engaged");
            self.engaged = true;
        }
        self.engaged
    }

    fn gap_us(&self) -> u64 {
        (self.p.max_gap_ms.max(0.0) * 1000.0) as u64
    }

    fn period_us(&self) -> f64 {
        1e6 / self.p.min_turn_rate.max(1e-3)
    }

    /// 结束当前 run，从 `now` 这一步重新开始（保留当前运动方向）。
    fn restart(&mut self, now: u64, why: RunEnd) {
        self.log_end(why);
        self.last_end = Some(why);
        self.start(now);
    }

    /// 从 `now` 这一步开始一次新的 run。
    fn start(&mut self, now: u64) {
        self.steps.clear();
        self.steps.push_back(Step {
            t: now,
            turn: 0.0,
            path: 0.0,
        });
        self.turn = 0.0;
        self.path = 0.0;
        self.dir = 0.0;
        self.counter = 0.0;
        self.engaged = false;
    }

    /// 速度、大小、短窗口转动是否仍像在画圈；不像时给出原因。
    fn consistent(&self, now: u64) -> Result<(), RunEnd> {
        let Some(start) = self.steps.front() else {
            return Ok(());
        };
        let period = self.period_us();
        // 最近一整圈：最新的、与当前转角相差满 360° 的点。`trim` 保证第一圈转满之后总能找到。
        match self.steps.iter().rev().find(|s| (self.turn - s.turn).abs() >= TAU) {
            Some(s) => {
                let r = (self.path - s.path) / TAU;
                if (now - s.t) as f64 > period {
                    return Err(RunEnd::SlowTurn);
                }
                if r < self.p.min_radius {
                    return Err(RunEnd::TooSmall);
                }
                if r > self.p.max_radius {
                    return Err(RunEnd::TooLarge);
                }
            }
            None => {
                // 第一圈未满：`start` 是 run 的起点。
                if (now - start.t) as f64 > period {
                    return Err(RunEnd::SlowTurn);
                }
                if self.path - start.path > TAU * self.p.max_radius {
                    return Err(RunEnd::TooLarge);
                }
            }
        }
        // 短窗口：最近 end_window 内转过的角度（run 开始不到一个窗口时不判定）。
        let window = self.p.end_window_ms.max(0.0) * 1000.0;
        if window > 0.0
            && let Some(s) = self.steps.iter().rev().find(|s| (now - s.t) as f64 >= window)
        {
            let need = TAU * self.p.min_turn_rate.max(1e-3) * (now - s.t) as f64 / 1e6 * self.p.end_min_fraction;
            if (self.turn - s.turn).abs() < need {
                return Err(RunEnd::Stalled);
            }
        }
        Ok(())
    }

    /// 只保留判定需要的点：最近一整圈之前的一个点，以及短窗口之前的一个点。
    ///
    /// 「一整圈之前」多留出 `max_counter_turn_deg`：真实鼠标的整数位移让方向有小的反向抖动，当前转角会比
    /// 最大值回退（最多回退这么多，再多就按反向结束）。只留恰好 360° 之前的点时，一次回退就找不到「最近
    /// 一整圈」，会被误当成「第一圈未满」去比较 run 的总路程而清零（2026-10-01 真实画圈中途断开的原因）。
    fn trim(&mut self) {
        let window = (self.p.end_window_ms.max(0.0) * 1000.0) as u64;
        let keep_turn = TAU + self.p.max_counter_turn_deg.max(0.0).to_radians();
        let now = self.steps.back().map_or(0, |s| s.t);
        while self.steps.len() > 2 {
            let second = self.steps[1];
            let rev_ok = self.dir != 0.0 && (self.turn - second.turn) * self.dir >= keep_turn;
            let win_ok = now.saturating_sub(second.t) >= window;
            if rev_ok && win_ok {
                self.steps.pop_front();
            } else {
                break;
            }
        }
    }
}

/// 把角度差折到 (-π, π]。
fn wrap(a: f64) -> f64 {
    let mut a = a % TAU;
    if a > PI {
        a -= TAU;
    } else if a <= -PI {
        a += TAU;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    const HZ: f64 = 125.0;

    /// 按轨迹函数（秒 → plane 位置）以 `HZ` 采样，逐个喂给识别器；返回每个采样时刻（秒）与是否触发。
    fn run(det: &mut CircleDetector, t0: f64, secs: f64, f: impl Fn(f64) -> (f64, f64)) -> Vec<(f64, bool)> {
        let n = (secs * HZ).round() as usize;
        let mut out = Vec::with_capacity(n);
        let mut prev = f(0.0);
        for i in 1..=n {
            let t = i as f64 / HZ;
            let p = f(t);
            let e = det.motion((p.0 - prev.0) / 686.0, (p.1 - prev.1) / 686.0, ((t0 + t) * 1e6) as u64);
            prev = p;
            out.push((t, e));
        }
        out
    }

    fn circle(r: f64, rev_per_s: f64, cw: bool) -> impl Fn(f64) -> (f64, f64) {
        let s = if cw { 1.0 } else { -1.0 };
        move |t| {
            let a = TAU * rev_per_s * t * s;
            (r * a.cos(), r * a.sin())
        }
    }

    fn first_engaged(v: &[(f64, bool)]) -> Option<f64> {
        v.iter().find(|(_, e)| *e).map(|(t, _)| *t)
    }

    #[test]
    fn third_turn_triggers_first_two_do_not() {
        for (r, rate, cw) in [
            (20.0, 2.5, true),
            (8.0, 3.0, false),
            (60.0, 2.0, true),
            (4.0, 4.0, true),
        ] {
            let mut d = CircleDetector::default();
            let v = run(&mut d, 0.0, 3.0 / rate, circle(r, rate, cw));
            let t = first_engaged(&v).unwrap_or_else(|| panic!("r={r} rate={rate} never engaged"));
            let turns = t * rate;
            // 第一、二圈不触发；进入第三圈（重采样与第一步的起点损失，最多晚几步）时触发。
            assert!(
                (2.0..2.2).contains(&turns),
                "r={r} rate={rate}: engaged at {turns:.3} turns"
            );
            assert!(v.last().unwrap().1, "still engaged while circling");
        }
    }

    #[test]
    fn keeps_engaged_while_circling_and_ends_after_stop() {
        let mut d = CircleDetector::default();
        let v = run(&mut d, 0.0, 4.0, circle(25.0, 2.5, true));
        let t = first_engaged(&v).unwrap();
        assert!(
            v.iter().filter(|(s, _)| *s > t).all(|(_, e)| *e),
            "drops out while circling"
        );
        assert!((2.0..2.2).contains(&d.turns()));
        // 停住：超过 max_gap 后 tick 结束。
        let last = (4.0 * 1e6) as u64;
        assert!(d.tick(last + 100_000));
        assert_eq!(d.deadline(), Some(last + 120_001));
        assert!(!d.tick(last + 121_000));
        assert_eq!(d.turns(), 0.0);
        assert_eq!(d.deadline(), None);
    }

    #[test]
    fn slow_circles_never_trigger() {
        for rate in [0.5, 1.0, 1.3] {
            let mut d = CircleDetector::default();
            let v = run(&mut d, 0.0, 6.0 / rate, circle(30.0, rate, true));
            assert_eq!(first_engaged(&v), None, "rate {rate}");
        }
    }

    #[test]
    fn arbitrary_motion_holds_until_stop_then_requires_new_circles() {
        let mut d = CircleDetector::default();
        run(&mut d, 0.0, 1.2, circle(20.0, 2.5, true));
        assert!(d.engaged());
        assert!(run(&mut d, 1.2, 1.0, circle(20.0, 0.1, false)).iter().all(|(_, e)| *e));
        // 非零位移远小于重采样步长，100 ms 间隔仍保持；每次都急转折返。
        for i in 1..=20 {
            assert!(d.motion(
                if i % 2 == 0 { 0.001 / 686.0 } else { -0.001 / 686.0 },
                0.0,
                2_200_000 + i * 100_000
            ));
        }
        assert!(d.tick(4_320_000));
        // 零位移不是运动，不能推迟 deadline。
        assert!(d.motion(0.0, 0.0, 4_320_000));
        assert!(!d.tick(4_320_001));
        assert_eq!(d.last_end(), Some(RunEnd::Pause));
        assert!(run(&mut d, 4.4, 1.0, |t| (t, 0.0)).iter().all(|(_, e)| !e));
        d.tick(5_600_000);
        let v = run(&mut d, 5.6, 1.2, circle(20.0, 2.5, true));
        assert!(first_engaged(&v).unwrap() * 2.5 >= 2.0);
    }

    #[test]
    fn motion_after_gap_resets_even_without_a_timer_tick() {
        let mut d = CircleDetector::default();
        run(&mut d, 0.0, 1.2, circle(20.0, 2.5, true));
        assert!(d.engaged());
        assert!(!d.motion(1.0 / 686.0, 0.0, 1_320_001));
        assert_eq!(d.last_end(), Some(RunEnd::Pause));
        assert_eq!(d.turns(), 0.0);
    }

    #[test]
    fn straight_lines_and_gentle_arcs_never_trigger() {
        let mut d = CircleDetector::default();
        assert_eq!(first_engaged(&run(&mut d, 0.0, 2.0, |t| (400.0 * t, 120.0 * t))), None);
        // 大弧线（半径 300/686 reference units，速度很快）：角速度低、半径超范围。
        let mut d = CircleDetector::default();
        assert_eq!(first_engaged(&run(&mut d, 0.0, 3.0, circle(300.0, 1.0, true))), None);
        // 往返扫过屏幕（之字形）。
        let mut d = CircleDetector::default();
        let zigzag = |t: f64| {
            let x = (t * 2.0).fract();
            (if (t * 2.0) as i64 % 2 == 0 { x } else { 1.0 - x } * 200.0, 50.0 * t)
        };
        assert_eq!(first_engaged(&run(&mut d, 0.0, 4.0, zigzag)), None);
    }

    #[test]
    fn back_and_forth_shaking_never_triggers() {
        for (amp, f) in [(30.0, 4.0), (60.0, 3.0), (10.0, 6.0)] {
            let mut d = CircleDetector::default();
            let v = run(&mut d, 0.0, 3.0, move |t| (amp * (TAU * f * t).sin(), 0.0));
            assert_eq!(first_engaged(&v), None, "shake {amp}plane {f}Hz");
            // 稍微斜一点、带一点竖直摆动的晃动（很扁的椭圆）。
            let mut d = CircleDetector::default();
            let v = run(&mut d, 0.0, 3.0, move |t| {
                (amp * (TAU * f * t).sin(), 1.0 * (TAU * f * t).cos())
            });
            assert_eq!(first_engaged(&v), None, "flat ellipse {amp}plane {f}Hz");
        }
    }

    #[test]
    fn figure_eight_and_reversal_never_trigger() {
        let mut d = CircleDetector::default();
        // 8 字：两个瓣的转向相反。
        let v = run(&mut d, 0.0, 4.0, |t| {
            let a = TAU * 1.5 * t;
            (30.0 * a.sin(), 15.0 * (2.0 * a).sin())
        });
        assert_eq!(first_engaged(&v), None, "figure eight");
        // 每 1.5 圈换一次方向。
        let mut d = CircleDetector::default();
        let mut t0 = 0.0;
        let mut any = false;
        for i in 0..4 {
            let v = run(&mut d, t0, 0.6, circle(20.0, 2.5, i % 2 == 0));
            any |= first_engaged(&v).is_some();
            t0 += 0.6;
        }
        assert!(!any, "reversing every 1.5 turns");
    }

    #[test]
    fn tiny_jitter_and_tiny_fast_circles_never_trigger() {
        // 手抖：±0.3/686 reference units 的伪随机抖动。
        let mut d = CircleDetector::default();
        let mut seed = 7u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 1000) as f64 / 1000.0 - 0.5
        };
        for i in 0..2000 {
            d.motion(rnd() * 0.6 / 686.0, rnd() * 0.6 / 686.0, i * 8000);
        }
        assert!(!d.engaged());
        // 半径 1/686 reference units 的快速小圈：低于最小半径。
        let mut d = CircleDetector::default();
        assert_eq!(first_engaged(&run(&mut d, 0.0, 3.0, circle(1.0, 4.0, true))), None);
    }

    #[test]
    fn noisy_and_drifting_circles_still_trigger() {
        // 椭圆 + 慢慢漂移 + 抖动，整数化的位移（模拟整数像素 × 0.2/686 reference units）。
        let mut d = CircleDetector::default();
        let mut prev = (35.0f64, 0.0f64);
        let mut acc = (0.0f64, 0.0f64);
        let mut engaged_at = None;
        for i in 1..=500 {
            let t = i as f64 / HZ;
            let a = TAU * 2.2 * t;
            let p = (35.0 * a.cos() + 20.0 * t + 0.4 * (13.0 * t).sin(), 22.0 * a.sin());
            // 0.2/686 reference units/像素，只发整数像素
            acc.0 += (p.0 - prev.0) / 0.2;
            acc.1 += (p.1 - prev.1) / 0.2;
            prev = p;
            let (px, py) = (acc.0.trunc(), acc.1.trunc());
            acc.0 -= px;
            acc.1 -= py;
            if d.motion(px * 0.2 / 686.0, py * 0.2 / 686.0, (t * 1e6) as u64) && engaged_at.is_none() {
                engaged_at = Some(t * 2.2);
            }
        }
        let turns = engaged_at.expect("engages");
        assert!((2.0..2.3).contains(&turns), "engaged at {turns}");
    }

    #[test]
    fn a_pause_mid_circle_restarts_the_count() {
        let mut d = CircleDetector::default();
        run(&mut d, 0.0, 0.7, circle(20.0, 2.5, true)); // 1.75 圈
        // 停 200 ms 再继续：从头计数，又要转满两圈。
        let v = run(&mut d, 0.9, 1.0, move |t| {
            let a = TAU * 2.5 * (t + 0.7);
            (20.0 * a.cos(), 20.0 * a.sin())
        });
        let t = first_engaged(&v).unwrap();
        assert!(t * 2.5 >= 2.0, "engaged after only {:.2} more turns", t * 2.5);
    }

    /// Synthetic integer-device positions: quantization introduces short reverse steps.
    fn quantized_circling() -> Vec<(u64, f64, f64)> {
        let position = circle(20.0, 2.5, true);
        let quantize = |t| {
            let (x, y) = position(t);
            ((x / 0.2).round() * 0.2, (y / 0.2).round() * 0.2)
        };
        let mut previous = quantize(0.0);
        (1..=2625)
            .map(|i| {
                let t = i as f64 / HZ;
                let next = quantize(t);
                let event = ((t * 1e6) as u64, next.0 - previous.0, next.1 - previous.1);
                previous = next;
                event
            })
            .collect()
    }

    /// 按路由的方式重放（每个事件前先检查停顿），返回每次状态变化：(秒, 是否触发, 清零原因)。
    fn replay(d: &mut CircleDetector, events: &[(u64, f64, f64)]) -> Vec<(f64, bool, Option<RunEnd>)> {
        let mut changes = Vec::new();
        let mut was = false;
        for &(t, dx, dy) in events {
            d.tick(t);
            let e = d.motion(dx / 686.0, dy / 686.0, t);
            if e != was {
                changes.push((t as f64 / 1e6, e, d.last_end()));
                was = e;
            }
        }
        changes
    }

    #[test]
    fn quantized_circling_engages_once_and_never_drops() {
        // 2026-10-01 用户连续画了 21 秒，安装版（a9cd897）在其中断开 6 次（日志里 2.2–4.0 s 一次的
        // draw=false）。原因：`trim` 只留恰好 360° 之前的点，整数位移的小反向抖动让当前转角回退一点后找不到
        // 「最近一整圈」，落进「第一圈未满」的分支拿 run 的总路程（> 2π × 150/686 reference units）比较而清零。
        // 此处用量化的合成轨迹覆盖相同的记账边界，原始录制夹具保存在仓库外。
        let events = quantized_circling();
        assert!(events.len() > 2500);
        let mut d = CircleDetector::default();
        let changes = replay(&mut d, &events);
        assert_eq!(changes.len(), 1, "engages once and never drops: {changes:?}");
        let (t, engaged, _) = changes[0];
        assert!(engaged);
        assert!(t < 1.0, "engaged at {t:.3}s");
        assert!(d.turns() >= 2.0);
        // 停手：停顿判定结束，原因是停顿。
        let last = events.last().unwrap().0;
        assert!(!d.tick(last + 121_000));
        assert_eq!(d.last_end(), Some(RunEnd::Pause));
    }

    #[test]
    fn end_reasons_are_recorded() {
        let mut d = CircleDetector::default();
        run(&mut d, 0.0, 1.2, circle(20.0, 2.5, true));
        d.reset();
        assert_eq!(d.last_end(), Some(RunEnd::External));
        let mut d = CircleDetector::default();
        run(&mut d, 0.0, 0.4, circle(20.0, 2.5, true));
        run(&mut d, 0.4, 0.5, circle(20.0, 2.5, false));
        assert!(matches!(d.last_end(), Some(RunEnd::CounterTurn | RunEnd::SharpTurn)));
        let mut d = CircleDetector::default();
        run(&mut d, 0.0, 3.0, circle(1.0, 4.0, true));
        assert_eq!(d.last_end(), Some(RunEnd::TooSmall));
    }

    #[test]
    fn wrap_is_symmetric() {
        assert!((wrap(3.0 * PI / 2.0) + PI / 2.0).abs() < 1e-12);
        assert!((wrap(-3.0 * PI / 2.0) - PI / 2.0).abs() < 1e-12);
        assert!((wrap(0.1) - 0.1).abs() < 1e-12);
    }
}
