//! Windows：模式切换前后「已经在路上」的鼠标移动（与平台无关的纯逻辑；Windows 截获线程使用，多设备模拟器
//! 用它模拟 Windows backend）。
//!
//! 低级钩子回调不是只由系统输入线程串行调用的：`SendInput` 注入的事件（包括模式切换用的标记）在注入方的
//! 线程里调用钩子，与鼠标的事件同时在路上。标记的回调里移动光标（停靠、回到落点）时，另一个 `pt` 已经算好的
//! 鼠标事件可能正排在截获线程上等着——它的 `pt` 是按切换之前的位置算的。Windows 实测（2026-09-29）：回到本机
//! 后第一步 `pt` = 停靠点 + 一步，而光标已在落点；按到达的先后放行它，光标跳回停靠点（屏幕中央）、之后从那里
//! 走，路由也以它为指针位置（「光标从中心开始移动」；新焦点机据此以为指针已远离入口边，4 ms 内就通知
//! 交出方 `LeftEntryEdge`，Mac 的边缘停留形同虚设）。离开时同理：按离开点算的一步会被当成
//! 「离开点 − 停靠点」那么大的一步发给对方。
//!
//! 所以切换后的一小段时间（[`STALE_WINDOW_US`]）里，按 `pt` 离哪个基准近判断这一步是按哪个算的
//! （[`computed_from_stale`]），位移按那个基准算；看到按现在的位置算的一步之后不再留意（之后的都是）。

use oink_foundation::Point;

/// 模式切换之后这么久之内（微秒），留意按切换之前的光标位置算出来的移动。
pub const STALE_WINDOW_US: u64 = 250_000;
/// 切换前后两个位置相距不到这么远（像素）时分不清一步是按哪个算的，不区分。
pub const STALE_MIN_JUMP_PX: f64 = 100.0;

/// `pt` 是按旧基准 `stale` 算的（而不是按现在的光标位置 `cur`）：离 `stale` 比离 `cur` 近。
/// 两者太近（[`STALE_MIN_JUMP_PX`]）时分不清，按现在的算。
pub fn computed_from_stale(pt: Point, cur: Point, stale: Point) -> bool {
    let d2 = |a: Point, b: Point| (a.x - b.x).powi(2) + (a.y - b.y).powi(2);
    d2(cur, stale) >= STALE_MIN_JUMP_PX * STALE_MIN_JUMP_PX && d2(pt, stale) < d2(pt, cur)
}

/// 最近一次模式切换之前的光标位置（系统给已经在路上的移动算 `pt` 时用的基准）。
#[derive(Clone, Copy, Debug, Default)]
pub struct StaleSteps {
    base: Option<(Point, u64)>,
}

impl StaleSteps {
    pub fn new() -> Self {
        Self::default()
    }

    /// 在时刻 `now_us` 切换了模式，切换之前光标在 `before`（离开时是离开点，回到本机时是停靠点）。
    pub fn switched(&mut self, before: Point, now_us: u64) {
        self.base = Some((before, now_us));
    }

    /// 不再留意（切换之前与之后是同一个位置，例如不带落点地回到本机）。
    pub fn clear(&mut self) {
        self.base = None;
    }

    /// 一个本机鼠标的移动：`pt` 是它的目标位置，`cur` 是它生效之前的光标位置。若是按切换之前的位置算的，
    /// 返回那个位置（位移应为 `pt − 它`，事件不能放行）。看到按现在的位置算的一步之后不再留意。
    pub fn classify(&mut self, pt: Point, cur: Point, now_us: u64) -> Option<Point> {
        let (base, since) = self.base?;
        if now_us.saturating_sub(since) > STALE_WINDOW_US || !computed_from_stale(pt, cur, base) {
            self.base = None;
            return None;
        }
        Some(base)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    /// Windows（4K，停靠点在屏幕中央），从底边回到本机。
    const PARK: Point = Point { x: 1920.0, y: 1080.0 };
    const ENTRY: Point = Point { x: 2988.0, y: 2154.0 };

    /// Windows 实测：回到本机、光标已在落点，路上的一步却是按停靠点算的。
    #[test]
    fn after_returning_a_step_from_the_park_point_is_stale() {
        assert!(computed_from_stale(p(1920.0, 1075.0), ENTRY, PARK));
        // 快速的一大步也分得清
        assert!(computed_from_stale(p(1920.0, 880.0), ENTRY, PARK));
        // 按落点算的一步
        assert!(!computed_from_stale(p(2988.0, 2148.0), ENTRY, PARK));
    }

    #[test]
    fn after_leaving_a_step_from_the_exit_point_is_stale() {
        // 从底边离开：光标已停靠在中央，路上的一步按离开点算（被系统夹在屏幕内）
        assert!(computed_from_stale(p(2990.0, 2159.0), PARK, ENTRY));
        assert!(!computed_from_stale(p(1922.0, 1086.0), PARK, ENTRY));
    }

    #[test]
    fn close_positions_are_not_told_apart() {
        assert!(!computed_from_stale(p(149.0, 100.0), p(100.0, 100.0), p(150.0, 100.0)));
    }

    #[test]
    fn stale_steps_until_the_first_fresh_one() {
        let mut s = StaleSteps::new();
        s.switched(PARK, 1_000);
        assert_eq!(s.classify(p(1920.0, 1074.0), ENTRY, 1_500), Some(PARK));
        assert_eq!(s.classify(p(1920.0, 1068.0), ENTRY, 2_500), Some(PARK));
        // 按落点算的一步：之后不再留意，哪怕又像是按停靠点算的
        assert_eq!(s.classify(p(2988.0, 2148.0), ENTRY, 3_000), None);
        assert_eq!(s.classify(p(1920.0, 1062.0), p(2988.0, 2148.0), 3_500), None);
    }

    #[test]
    fn stale_steps_expire() {
        let mut s = StaleSteps::new();
        s.switched(PARK, 0);
        assert_eq!(s.classify(p(1920.0, 1074.0), ENTRY, STALE_WINDOW_US + 1), None);
    }
}
