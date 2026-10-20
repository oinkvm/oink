//! macOS：程序移动光标（`CGWarpMouseCursorPosition`，不产生事件）之后，下一个硬件移动事件的
//! `DeltaX/Y` includes the warp displacement. Observed on macOS 26.6:
//! - F1：warp 恰好算进 warp 之后的**下一个**移动事件一次，空闲多久都一样。
//! - F2：算进去的量 = warp 目标 − 上一个事件的位置（HID 层的指针位置），不是 `CGEventCreate` 读到的光标。
//! - F3：warp 到光标当前所在的点会被系统忽略：不算进位移，也不重置事件位置。
//! - F4：warp 之后约 250 ms 事件位置冻结（`CGSetLocalEventsSuppressionInterval` 可去掉）。
//! - F5：远程模式（隐藏 + 解耦 + 吞掉）下事件位置**不是**光标位置：光标不动，事件位置按位移累加，
//!   并可能卡住。
//!
//! 所以位移只取事件的位移字段；远程模式期间不 warp（交出焦点时原地冻结，见 `macos/cursor.rs`），这里
//! 只剩下回到本机时的那一次 warp（放到进入点）：此时已经关联、事件位置可信，warp 之后的第一个移动
//! 事件的位移取「事件位置 − 进入点」（位移字段里含的是「进入点 − 远程期间漂走的事件位置」，没有意义）。
//! 事件位置还冻结在进入点（F4 没去掉）时结果是 0：丢一步，不会反向。
//!
//! **warp 还没对这个事件生效**（实测 2026-09-29 本机真实触摸板：warp 前一刻产生的事件时间戳晚于登记的
//! 时刻，位移字段 (2, 1) 不含 warp，事件位置还是远程期间的旧值，「位置 − 进入点」= (−17, 99)，焦点立刻
//! 又被推回对面）：生效时「位置 − 进入点」是手指的一步、位移字段多出整段 warp；没生效时位移字段是手指的
//! 一步、「位置 − 进入点」多出「旧位置 − 进入点」。所以两者取模较小的一个：warp 距离大时不会选错，
//! 距离小时两者都接近手指的一步。选了位移字段时 warp 仍待修正（下一个事件才含它），最多再等
//! [`MAX_PENDING_EVENTS`] 个事件。不按时间过期：空闲多久，warp 都算进下一个事件（F1）。
//!
//! 原地 warp（目标与 warp 前的光标相差不到 [`MIN_WARP_PT`]）被系统忽略（F3），事件位置不重置，
//! 「位置 − 目标」会是远程期间的漂移。冻结在离开点、又从同一处回来时正是这种情况：调用方先 warp 到
//! 旁边的 [`nudge_point`] 再 warp 到进入点，保证 warp 生效后才登记。
//!
//! **回到本机时光标沿旧位置接着走**（用户报告「回到本机后光标走得比预期远得多」，macOS 实测：硬件输入
//! 4/21 次、合成输入 1/11 次）：远程期间按旧位置算好、warp 之后才被放行的事件把光标带回旧位置（见
//! `macos/tap.rs` 的 `restore_to`）。调用方在 warp 与关联做完的那一刻登记 warp，并吞掉时间戳更早的
//! 移动事件（[`WarpDeltaFix::predates_warp`]）；之后第一个事件若仍离进入点很远、位移字段里又没有
//! warp，只记一条警告（[`WarpDeltaFix::take_mismatch`]），不改变行为。
//!
//! 时间戳早于 warp 的事件不是 warp 之后的事件：原样使用，不消耗待修正的 warp。
//! 自己注入（post）的移动事件会重新确定位移基准：清除待修正的 warp。
//! CG 绝对注入的 session 回执登记到 [`WarpDeltaFix::posted_absolute_motion`]，扣除待折入的程序位移。
//! 原生绝对 HID 移动已经推进 HID 位置：其回执调用 [`WarpDeltaFix::native_absolute_motion`] 更新基准，
//! 不能再从下一次物理移动反减旧物理位置到注入位置的距离。
//! 进入 Remote 只取消本机 warp 修正，保留该折入量；冻结挪出感应区时用最终位置替换它，不能重复相加。
//!
//! **边缘停留结束时投递的移动**：交出焦点后光标在离开的那条边上停留（隐藏、
//! 仍关联）；指针离入口边 10 mm 后，backend 先解耦、再**投递**一个带注入标记的移动事件把光标挪出 Dock、触发角、
//! 全屏菜单栏的感应区（[`clear_of_active_edge_zones`]）。实测它与 warp 一样算进下一个硬件移动事件，量 = 目标 −
//! 上一个事件的位置（F2）。投递是异步的，何时生效看不到时刻，但截获回调按顺序看到它：回调认出这个事件时调用
//! [`WarpDeltaFix::posted_move`]，之后的第一个硬件移动事件减去「目标 − 它之前最后一个事件的位置」，只减一次。
//! 不用「事件位置 − 目标」：解耦时事件位置被夹在屏幕边上（F8），推着边的那一步会算错（实测 −6 被算成 −3）。
//!
//! 这里是与平台无关的纯逻辑（macOS 截获线程使用；多设备模拟器用它模拟 macOS 的系统行为）。

use oink_foundation::{Point, Rect};
use serde::{Deserialize, Serialize};

/// Observation only: the exact state used to repair a native motion sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeltaSnapshot {
    pub hid_location: Option<Point>,
    pub fold: Option<((f64, f64), Point)>,
    pub warp: Option<(Point, u64)>,
    pub drop_next: bool,
}

/// warp 目标与 warp 前光标位置相差不到这个距离（pt）时，系统忽略这次 warp（F3），不登记。
pub const MIN_WARP_PT: f64 = 1.0;

/// warp 是否会被系统当作真的移动了光标（否则忽略，见 F3）。`cursor` 是 warp 前读到的光标位置。
pub fn warp_moves_cursor(cursor: Point, to: Point) -> bool {
    (to.x - cursor.x).hypot(to.y - cursor.y) >= MIN_WARP_PT
}

/// 原地 warp 会被忽略、事件位置不重置（F3）。回到本机时若进入点就是光标冻结的位置，先 warp 到
/// 朝 `toward`（屏幕内部，如参考屏中央）偏 [`NUDGE_PT`] 的点，再 warp 到进入点，让系统真的移动光标、
/// 重置事件位置。返回那个中间点。
pub fn nudge_point(at: Point, toward: Point) -> Point {
    let (dx, dy) = (toward.x - at.x, toward.y - at.y);
    let len = dx.hypot(dy);
    if len < 1e-9 {
        return Point::new(at.x + NUDGE_PT, at.y);
    }
    Point::new(at.x + dx / len * NUDGE_PT, at.y + dy / len * NUDGE_PT)
}

/// [`nudge_point`] 偏开的距离（pt）。
pub const NUDGE_PT: f64 = 4.0;

/// 一次待修正的 warp。
#[derive(Clone, Copy, Debug)]
struct Warp {
    to: Point,
    /// warp（与重新关联）做完的时刻（与事件时间戳同一时钟，纳秒）。
    at_ns: u64,
    /// warp 之后已经送到、但判断为 warp 还没生效的事件数。
    seen: u32,
}

/// 减去投递的移动之后，事件位置离「目标 + 修正后的位移」超过这个距离（pt）：记一条警告。
pub const FOLD_CHECK_PT: f64 = 3.0;

/// warp 之后第一个事件的位置离进入点超过这个距离（pt）、位移字段里又没有 warp：记一条警告。
pub const MISMATCH_PT: f64 = 20.0;

/// warp 之后最多这么多个事件还判断为「warp 没生效」；之后放弃修正。
pub const MAX_PENDING_EVENTS: u32 = 2;

/// 修正 warp 之后第一个移动事件的位移。见模块说明。
#[derive(Clone, Debug, Default)]
pub struct WarpDeltaFix {
    warp: Option<Warp>,
    /// warp 之后的事件位置离进入点很远、位移字段里又没有 warp：光标没有从进入点出发（只供记录）。
    mismatch: Option<Point>,
    /// 最近一个 HID 层移动事件（硬件的，或 `IOHIDPostEvent` 投递的）的位置。系统算下一个 HID 事件的位移字段
    /// 时从这里算起：自己注入（`CGEventPost`）的移动、warp 都会移动光标，但不改变这个起点（实测，见
    /// [`WarpDeltaFix::posted_move`]）。
    hid_loc: Option<Point>,
    /// 自己投递的移动（[`WarpDeltaFix::posted_move`]）要从下一个 HID 移动事件里减去的量，以及投递的目标。
    fold: Option<((f64, f64), Point)>,
    /// 减去之后的检查没通过：下一个事件位置不是「目标 + 修正后的位移」（只供记录）。
    leak: Option<FoldLeak>,
    /// 投递了移动、却不知道系统从哪里算（从没见过 HID 事件）：下一个 HID 移动事件整个丢掉（只丢手指的一步）。
    drop_next: bool,
}

/// 冻结时投递的移动没有被干净地减掉（只供记录，见 [`WarpDeltaFix::take_leak`]）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoldLeak {
    pub target: Point,
    pub raw: (f64, f64),
    pub fixed: (f64, f64),
    pub at: Point,
}

impl WarpDeltaFix {
    pub fn snapshot(&self) -> DeltaSnapshot {
        DeltaSnapshot {
            hid_location: self.hid_loc,
            fold: self.fold,
            warp: self.warp.map(|w| (w.to, w.at_ns)),
            drop_next: self.drop_next,
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// 程序在时刻 `at_ns`（与事件时间戳同一时钟）把光标 warp 到了 `to`，并且之后光标与鼠标关联
    /// （事件位置可信）。只登记确实移动了光标的 warp（[`warp_moves_cursor`]）。
    pub fn warped(&mut self, to: Point, at_ns: u64) {
        self.fold = None;
        self.drop_next = false;
        self.warp = Some(Warp { to, at_ns, seen: 0 });
    }

    /// 截获回调按顺序看到了自己投递、目标为 `to` 的那个移动事件（边缘停留结束时挪出感应区，见模块说明）：
    /// 下一个 HID 移动事件减去「`to` − 最后一个 HID 移动事件的位置」。
    ///
    /// 起点是最后一个 **HID** 事件，不是最后一个事件：实测（2026-09-29 本机，被控期间有很多注入的移动）冻结时
    /// 投递到 (863.7, 30)，一分多钟后第一个触摸板事件的位移字段是 (−173, −467) = 手指 (−8.5, −0.6) +
    /// (863.7 − 1028.2, 30 − 496.4)，(1028.2, 496.4) 正是被控之前最后一个触摸板事件的位置；按最后一个（注入的）
    /// 事件位置算只减了 (0, 30)，(−164, −437) 发给了 Windows。不知道 HID 位置时不登记（返回 `false`）。
    pub fn posted_move(&mut self, to: Point) -> bool {
        let Some(from) = self.hid_loc else {
            // 被控的 Mac 从 Oink 启动以来没用过自己的鼠标：不知道量，就丢掉下一个 HID 事件。
            self.fold = None;
            self.drop_next = true;
            return false;
        };
        self.drop_next = false;
        self.fold = Some(((to.x - from.x, to.y - from.y), to));
        true
    }

    /// 取出「冻结时投递的移动没有被干净地减掉」的记录。只用于记录日志。
    pub fn take_leak(&mut self) -> Option<FoldLeak> {
        self.leak.take()
    }

    /// 时间戳为 `ts_ns` 的事件是否产生在待修正的 warp 做完之前（调用方把这种事件吞掉，见
    /// `macos/tap.rs` 的 `restore_to`）。
    pub fn predates_warp(&self, ts_ns: u64) -> bool {
        self.warp.is_some_and(|w| ts_ns != 0 && ts_ns < w.at_ns)
    }

    /// 取出「回到本机后光标没有从进入点出发」的记录（进入点），只取一次。只用于记录日志。
    pub fn take_mismatch(&mut self) -> Option<Point> {
        self.mismatch.take()
    }

    /// 截获到自己注入的、时间戳为 `ts_ns` 的移动事件：系统的位移基准随之更新，之前的 warp 不再修正。
    /// 时间戳早于登记的 warp 做完的时刻的不算（在路上的旧注入）。
    pub fn posted(&mut self, ts_ns: u64) {
        if self.warp.is_some_and(|w| ts_ns < w.at_ns) {
            return;
        }
        self.warp = None;
    }

    /// An accepted CG cursor placement seen by the session tap. Unlike native
    /// HID movement, CG posting can leave displacement for the next HID sample.
    pub fn posted_absolute_motion(&mut self, at: Point, ts_ns: u64) {
        if self.warp.is_some_and(|w| ts_ns < w.at_ns) {
            return;
        }
        self.posted(ts_ns);
        self.posted_move(at);
    }

    /// An accepted native absolute HID move advances the HID position itself.
    pub fn native_absolute_motion(&mut self, at: Point, ts_ns: u64) {
        if self.warp.is_some_and(|w| ts_ns < w.at_ns) {
            return;
        }
        // macOS 26.6.2 physical-trackpad evidence has raw CG delta (0, 0)
        // after native HID movement and freezing. Treating that movement as CG
        // left an old physical-to-native fold and manufactured a large reverse
        // delta. The native event consumes that displacement, so later CG
        // placement must also count from this delivered/quantized HID position.
        self.clear();
        self.hid_loc = Some(at);
    }

    /// Entering Remote cancels a Local warp repair, while a CG absolute post's
    /// displacement still belongs to the next physical event. A subsequent
    /// freeze relocation replaces that fold using the same physical base.
    pub fn enter_remote(&mut self) {
        self.warp = None;
    }

    /// 显式放弃全部待修正的 warp 与投递的移动。HID 位置照旧记着。
    pub fn clear(&mut self) {
        self.warp = None;
        self.fold = None;
        self.drop_next = false;
    }

    /// 是否有 [`WarpDeltaFix::posted_move`] 登记、还没减去的投递的移动。
    pub fn fold_pending(&self) -> bool {
        self.fold.is_some() || self.drop_next
    }

    /// 是否有待修正的 warp 或投递的移动。
    pub fn pending(&self) -> bool {
        self.warp.is_some() || self.fold.is_some() || self.drop_next
    }

    /// 截获到一个移动事件：`raw` 是位移字段，`at` 是事件位置，`ts_ns` 是事件时间戳（不知道时传
    /// `u64::MAX`，当作 warp 之后的事件），`hid`：HID 层的事件（硬件或 `IOHIDPostEvent`；其他程序用
    /// `CGEventPost` 投递的不是，它们自带位移字段，不含 warp / 投递的移动）。返回位移。
    pub fn motion(&mut self, raw: (f64, f64), at: Point, ts_ns: u64, hid: bool) -> (f64, f64) {
        if !hid {
            if self.fold_pending() {
                return raw;
            }
        } else {
            self.hid_loc = Some(at);
            if std::mem::take(&mut self.drop_next) {
                return (0.0, 0.0);
            }
            if let Some(((fx, fy), to)) = self.fold.take() {
                let fixed = (raw.0 - fx, raw.1 - fy);
                // 检查（不改变行为）：解耦时 HID 位置从投递的目标起按位移累加，这个事件的位置应当是
                // 「目标 + 手指的一步」（贴着屏幕边时会被夹住，偏差不大）。
                let expect = Point::new(to.x + fixed.0, to.y + fixed.1);
                if (expect.x - at.x).hypot(expect.y - at.y) > FOLD_CHECK_PT {
                    self.leak = Some(FoldLeak {
                        target: to,
                        raw,
                        fixed,
                        at,
                    });
                }
                return fixed;
            }
        }
        let Some(w) = &mut self.warp else {
            return raw;
        };
        if ts_ns < w.at_ns {
            return raw;
        }
        let by_loc = (at.x - w.to.x, at.y - w.to.y);
        if by_loc.0.hypot(by_loc.1) <= raw.0.hypot(raw.1) {
            self.warp = None;
            return by_loc;
        }
        // warp 还没对这个事件生效：位移字段就是手指的一步。
        if w.seen == 0 && by_loc.0.hypot(by_loc.1) > MISMATCH_PT {
            self.mismatch = Some(w.to);
        }
        w.seen += 1;
        if w.seen >= MAX_PENDING_EVENTS {
            self.warp = None;
        }
        raw
    }
}

// ---------------- 屏幕边缘的感应区（边缘停留） ----------------

/// 光标离一条外缘（外面没有本机其他屏幕的边）不到这个距离（pt）算停在这条边上：交出焦点时停在外缘上才
/// 停留（隐藏、不冻结），否则立即冻结。
pub const EDGE_TOUCH_PT: f64 = 2.0;
/// 自动隐藏 Dock 的激活带安全距离（pt）：距边 2 pt 以内触发（底边实测；左右两侧待实测）。
/// 普通左右下边不因这个常量移动；生产路径还要求实际 Dock 范围或已启用触发角。
pub const EDGE_CLEAR_PT: f64 = 3.0;
/// 离顶边至少这么远（pt）：全屏时菜单栏的触发区约 29 pt（实测 y = 0–28 出现）。
pub const TOP_CLEAR_PT: f64 = 30.0;
/// 离两条相邻的外缘都不到这个距离（pt）算在触发角里，两个方向都挪到这个距离之外。触发角的范围待实测；
/// 在它不大于 [`EDGE_CLEAR_PT`] 时，挪出边缘就已经挪出了触发角。
pub const CORNER_CLEAR_PT: f64 = 3.0;

/// `p` 所在的显示器；不在任何显示器内（浮点误差、刚好在右 / 下边界上）时取最近的一块。
fn display_of(p: Point, displays: &[Rect]) -> Option<Rect> {
    if let Some(d) = displays.iter().find(|d| d.contains(p)) {
        return Some(*d);
    }
    displays.iter().copied().min_by(|a, b| {
        let da = dist_to(p, a);
        let db = dist_to(p, b);
        da.total_cmp(&db)
    })
}

fn dist_to(p: Point, r: &Rect) -> f64 {
    let c = r.clamp(p);
    (c.x - p.x).hypot(c.y - p.y)
}

/// `p` 附近四条边哪些是外缘（紧挨着边外 1 pt 的点不在任何显示器里）：左、右、上、下。
fn outer_edges(p: Point, d: &Rect, displays: &[Rect]) -> [bool; 4] {
    let outside = |q: Point| !displays.iter().any(|r| r.contains(q));
    [
        outside(Point::new(d.x - 1.0, p.y)),
        outside(Point::new(d.right() + 1.0, p.y)),
        outside(Point::new(p.x, d.y - 1.0)),
        outside(Point::new(p.x, d.bottom() + 1.0)),
    ]
}

/// 光标 `p` 是否停在本机屏幕排列的外缘上（[`EDGE_TOUCH_PT`] 以内）。`displays`：本机所有显示器（全局坐标）。
pub fn at_outer_edge(p: Point, displays: &[Rect]) -> bool {
    let Some(d) = display_of(p, displays) else {
        return false;
    };
    let [l, r, t, b] = outer_edges(p, &d, displays);
    let k = EDGE_TOUCH_PT;
    (l && p.x - d.x < k) || (r && d.right() - p.x < k) || (t && p.y - d.y < k) || (b && d.bottom() - p.y < k)
}

/// 冻结时离各条外缘至少多远（pt）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeClearance {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}

impl Default for EdgeClearance {
    /// 只挪出触发区：左右下 [`EDGE_CLEAR_PT`]，顶边 [`TOP_CLEAR_PT`]。
    fn default() -> Self {
        Self {
            left: EDGE_CLEAR_PT,
            right: EDGE_CLEAR_PT,
            top: TOP_CLEAR_PT,
            bottom: EDGE_CLEAR_PT,
        }
    }
}

/// 自动隐藏的 Dock 在哪条边。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockSide {
    Bottom,
    Left,
    Right,
}

/// Dock 弹出后，光标要离开 Dock 的区域它才收起（实测：Dock 顶边下方 10 pt 不收、上方 10 pt 约 146 ms 收起）；
/// Dock 区域的厚度约为图标大小加上下边距。冻结点离 Dock 那条边至少 `图标大小 + DOCK_MARGIN_PT`。
pub const DOCK_MARGIN_PT: f64 = 40.0;

impl EdgeClearance {
    /// Dock 在 `side`、图标（放大时取放大后的）大小 `tile_pt`：那一侧离开 Dock 区域，其他同默认。
    pub fn with_dock(side: DockSide, tile_pt: f64) -> Self {
        let dock = EDGE_CLEAR_PT.max(tile_pt + DOCK_MARGIN_PT);
        let mut c = Self::default();
        match side {
            DockSide::Bottom => c.bottom = dock,
            DockSide::Left => c.left = dock,
            DockSide::Right => c.right = dock,
        }
        c
    }
}

/// The actual Dock list rectangle in global Quartz coordinates (including when offscreen).
#[derive(Clone, Copy, Debug)]
pub struct DockZone {
    pub side: DockSide,
    pub frame: Rect,
    pub tile_pt: f64,
}

/// Freeze clearance only for active native zones. Ordinary side/bottom edges stay in place.
/// Top menu clearance is intentionally preserved, even outside fullscreen.
pub fn clear_of_active_edge_zones(
    p: Point,
    displays: &[Rect],
    dock: Option<DockZone>,
    corners: &[oink_foundation::input::ScreenCorner],
) -> Point {
    use oink_foundation::input::ScreenCorner;
    let Some(d) = display_of(p, displays) else { return p };
    let [l, r, t, b] = outer_edges(p, &d, displays);
    let mut q = p;
    if t && p.y - d.y < TOP_CLEAR_PT && d.h > TOP_CLEAR_PT {
        q.y = d.y + TOP_CLEAR_PT;
    }
    for corner in corners {
        let (left, top) = match corner {
            ScreenCorner::TopLeft => (true, true),
            ScreenCorner::TopRight => (false, true),
            ScreenCorner::BottomLeft => (true, false),
            ScreenCorner::BottomRight => (false, false),
        };
        let dx = if left { p.x - d.x } else { d.right() - p.x };
        let dy = if top { p.y - d.y } else { d.bottom() - p.y };
        if (if left { l } else { r })
            && (if top { t } else { b })
            && dx < CORNER_CLEAR_PT
            && dy < CORNER_CLEAR_PT
            && d.w > 2.0 * CORNER_CLEAR_PT
            && d.h > 2.0 * TOP_CLEAR_PT
        {
            q.x = if left {
                d.x + CORNER_CLEAR_PT
            } else {
                d.right() - CORNER_CLEAR_PT
            };
            q.y = if top {
                d.y + TOP_CLEAR_PT
            } else {
                d.bottom() - CORNER_CLEAR_PT
            };
        }
    }
    if let Some(DockZone { side, frame, tile_pt }) = dock {
        let anchor = match side {
            DockSide::Bottom => Point::new(frame.x + frame.w / 2.0, frame.y - 1.0),
            DockSide::Left => Point::new(frame.right() + 1.0, frame.y + frame.h / 2.0),
            DockSide::Right => Point::new(frame.x - 1.0, frame.y + frame.h / 2.0),
        };
        if display_of(anchor, displays) == Some(d) {
            let (outer, tangent, distance, depth, size) = match side {
                DockSide::Bottom => (
                    b,
                    p.x >= frame.x && p.x <= frame.right(),
                    d.bottom() - p.y,
                    d.bottom() - frame.y,
                    frame.h,
                ),
                DockSide::Left => (
                    l,
                    p.y >= frame.y && p.y <= frame.bottom(),
                    p.x - d.x,
                    frame.right() - d.x,
                    frame.w,
                ),
                DockSide::Right => (
                    r,
                    p.y >= frame.y && p.y <= frame.bottom(),
                    d.right() - p.x,
                    d.right() - frame.x,
                    frame.w,
                ),
            };
            let clearance = (tile_pt + DOCK_MARGIN_PT)
                .max(size + EDGE_CLEAR_PT)
                .max(depth + EDGE_CLEAR_PT);
            if outer && tangent && distance < depth.max(EDGE_CLEAR_PT) {
                match side {
                    DockSide::Bottom if clearance < d.h => q.y = q.y.min(d.bottom() - clearance),
                    DockSide::Left if clearance < d.w => q.x = q.x.max(d.x + clearance),
                    DockSide::Right if clearance < d.w => q.x = q.x.min(d.right() - clearance),
                    _ => {}
                }
            }
        }
    }
    q
}

/// 旧的全外缘策略，保留供显式策略比较。生产冻结使用 [`clear_of_active_edge_zones`]。
/// 按默认距离挪出全部外缘；不在范围内时原样返回。
pub fn clear_of_edge_zones(p: Point, displays: &[Rect]) -> Point {
    clear_of_edge_zones_by(p, displays, EdgeClearance::default())
}

/// 同 [`clear_of_edge_zones`]，各条外缘的距离取 `k`。只看外缘：挨着本机另一块屏幕的边没有这些行为。
/// 在角上（离两条外缘都不到 [`CORNER_CLEAR_PT`]）时两个方向都挪开。
pub fn clear_of_edge_zones_by(p: Point, displays: &[Rect], k: EdgeClearance) -> Point {
    let Some(d) = display_of(p, displays) else {
        return p;
    };
    let p = d.clamp(p);
    let [l, r, t, b] = outer_edges(p, &d, displays);
    let near = |outer: bool, dist: f64, k: f64| outer && dist < k;
    let c = CORNER_CLEAR_PT;
    let corner = (near(l, p.x - d.x, c) || near(r, d.right() - p.x, c))
        && (near(t, p.y - d.y, c) || near(b, d.bottom() - p.y, c));
    let at_least = |v: f64| if corner { v.max(c) } else { v };
    let (kl, kr, kt, kb) = (at_least(k.left), at_least(k.right), at_least(k.top), at_least(k.bottom));
    let mut q = p;
    // 屏幕太小（放不下两侧的距离）时不挪：没有意义，也不会越出屏幕。
    if d.w > kl + kr {
        if near(l, q.x - d.x, kl) {
            q.x = d.x + kl;
        } else if near(r, d.right() - q.x, kr) {
            q.x = d.right() - kr;
        }
    }
    if d.h > kt + kb {
        if near(t, q.y - d.y, kt) {
            q.y = d.y + kt;
        } else if near(b, d.bottom() - q.y, kb) {
            q.y = d.bottom() - kb;
        }
    }
    q
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    #[test]
    fn active_zones_leave_ordinary_edges_and_inactive_corners_in_place() {
        let d = [Rect::new(0.0, 0.0, 1000.0, 800.0)];
        for at in [p(0.0, 400.0), p(999.999, 400.0), p(500.0, 799.999), p(999.999, 799.999)] {
            assert_eq!(clear_of_active_edge_zones(at, &d, None, &[]), at);
        }
        assert_eq!(clear_of_active_edge_zones(p(500.0, 0.0), &d, None, &[]), p(500.0, 30.0));
        assert_eq!(
            clear_of_active_edge_zones(
                p(999.999, 799.999),
                &d,
                None,
                &[oink_foundation::input::ScreenCorner::BottomRight]
            ),
            p(997.0, 797.0)
        );
    }

    #[test]
    fn active_dock_uses_actual_side_span_and_activation_depth() {
        let d = [Rect::new(0.0, 0.0, 1000.0, 800.0)];
        let dock = DockZone {
            side: DockSide::Bottom,
            frame: Rect::new(200.0, 800.0, 600.0, 90.0),
            tile_pt: 64.0,
        };
        assert_eq!(
            clear_of_active_edge_zones(p(500.0, 799.999), &d, Some(dock), &[]),
            p(500.0, 696.0)
        );
        for at in [p(100.0, 799.999), p(999.999, 400.0), p(500.0, 750.0)] {
            assert_eq!(clear_of_active_edge_zones(at, &d, Some(dock), &[]), at);
        }
        for (side, frame, at, expected) in [
            (
                DockSide::Left,
                Rect::new(-90.0, 200.0, 90.0, 400.0),
                p(0.0, 400.0),
                p(104.0, 400.0),
            ),
            (
                DockSide::Right,
                Rect::new(1000.0, 200.0, 90.0, 400.0),
                p(999.999, 400.0),
                p(896.0, 400.0),
            ),
        ] {
            let side_dock = DockZone { side, frame, ..dock };
            assert_eq!(clear_of_active_edge_zones(at, &d, Some(side_dock), &[]), expected);
            let outside_span = p(at.x, 700.0);
            assert_eq!(
                clear_of_active_edge_zones(outside_span, &d, Some(side_dock), &[]),
                outside_span
            );
        }
        let shown = DockZone {
            frame: Rect::new(200.0, 710.0, 600.0, 90.0),
            ..dock
        };
        assert_eq!(
            clear_of_active_edge_zones(p(500.0, 750.0), &d, Some(shown), &[]),
            p(500.0, 696.0)
        );
    }

    #[test]
    fn active_zones_preserve_internal_seams_and_other_displays() {
        let d = [
            Rect::new(0.0, 0.0, 1000.0, 800.0),
            Rect::new(1000.0, 0.0, 1000.0, 800.0),
        ];
        let dock = DockZone {
            side: DockSide::Right,
            frame: Rect::new(1000.0, 200.0, 90.0, 400.0),
            tile_pt: 64.0,
        };
        assert_eq!(
            clear_of_active_edge_zones(p(999.999, 400.0), &d, Some(dock), &[]),
            p(999.999, 400.0)
        );
        assert_eq!(
            clear_of_active_edge_zones(p(1999.999, 400.0), &d, Some(dock), &[]),
            p(1999.999, 400.0)
        );
    }

    #[test]
    fn dock_clearance_uses_the_external_displays_own_bottom_above_or_below_primary() {
        for y in [-1080.0, 1117.0] {
            let external = Rect::new(1560.0, y, 1920.0, 1080.0);
            let displays = [Rect::new(0.0, 0.0, 1728.0, 1117.0), external];
            let source = p(2520.0, external.bottom() - 0.02);
            for frame_y in [external.bottom(), external.bottom() - 90.0] {
                let dock = DockZone {
                    side: DockSide::Bottom,
                    frame: Rect::new(2200.0, frame_y, 640.0, 90.0),
                    tile_pt: 63.0,
                };
                assert_eq!(
                    clear_of_active_edge_zones(source, &displays, Some(dock), &[]),
                    p(2520.0, external.bottom() - 103.0)
                );
            }
            // A Dock observed on the primary display must not be applied to
            // the external display, even when its bottom is at global y=0.
            let primary_dock = DockZone {
                side: DockSide::Bottom,
                frame: Rect::new(500.0, 1027.0, 728.0, 90.0),
                tile_pt: 63.0,
            };
            assert_eq!(
                clear_of_active_edge_zones(source, &displays, Some(primary_dock), &[]),
                source
            );
        }
    }

    #[test]
    fn external_dock_clears_both_axes_at_active_corners_but_preserves_internal_seams() {
        let displays = [
            Rect::new(0.0, 0.0, 1728.0, 1117.0),
            Rect::new(1560.0, -1080.0, 1920.0, 1080.0),
        ];
        let dock = DockZone {
            side: DockSide::Bottom,
            frame: Rect::new(1560.0, -90.0, 1920.0, 90.0),
            tile_pt: 128.0, // Magnified size already selected by the backend.
        };
        assert_eq!(
            clear_of_active_edge_zones(
                p(3479.98, -0.02),
                &displays,
                Some(dock),
                &[oink_foundation::input::ScreenCorner::BottomRight]
            ),
            p(3477.0, -168.0)
        );
        let seam = p(1600.0, -0.02);
        assert_eq!(clear_of_active_edge_zones(seam, &displays, Some(dock), &[]), seam);
    }

    #[test]
    fn untouched_without_warp() {
        let mut f = WarpDeltaFix::new();
        assert_eq!(f.motion((3.0, -4.0), p(100.0, 100.0), 10, true), (3.0, -4.0));
        assert_eq!(f.motion((3.0, -4.0), p(103.0, 96.0), 20, true), (3.0, -4.0));
    }

    /// 本次 bug（F6 的原始序列）：远程模式下事件位置卡在 P + (−4, 0)，位移字段依次 +4、(0, +4)、−4。
    /// 远程模式不 warp、不登记，输出必须跟着位移字段——不能是「位置 − 停靠点」这个常量。
    #[test]
    fn stuck_location_in_remote_mode_follows_the_delta_field() {
        let mut f = WarpDeltaFix::new();
        let stuck = p(860.0, 559.0);
        let mut t = 1_000;
        for raw in std::iter::repeat_n((4.0, 0.0), 40)
            .chain(std::iter::repeat_n((0.0, 4.0), 20))
            .chain(std::iter::repeat_n((-4.0, 0.0), 20))
        {
            t += 10;
            assert_eq!(f.motion(raw, stuck, t, true), raw);
        }
    }

    /// 原地 warp（目标 = 当前光标）被系统忽略（F3）：调用方不登记，之后位移照用字段——即使事件位置
    /// 是远程期间漂走的值。
    #[test]
    fn in_place_warp_is_not_registered() {
        let cursor = p(0.0, 500.0);
        assert!(!warp_moves_cursor(cursor, p(0.4, 500.5)));
        assert!(warp_moves_cursor(cursor, p(1.0, 500.0)));
        assert!(warp_moves_cursor(cursor, p(864.0, 558.5)));
        let mut f = WarpDeltaFix::new();
        // 没有登记：事件位置（远程期间漂到 −300）不参与。
        assert_eq!(f.motion((5.0, 0.0), p(-300.0, 520.0), 100, true), (5.0, 0.0));
    }

    /// 回到本机：放到进入点 (1727, 400)。位移字段里是「进入点 − 远程期间漂走的事件位置」加手指，
    /// 第一个事件改用「事件位置 − 进入点」，只修正一次（F1）。
    #[test]
    fn restore_warp_uses_location_once() {
        let mut f = WarpDeltaFix::new();
        f.warped(p(1727.0, 400.0), 1_000);
        assert!(f.pending());
        let d = f.motion((1500.0, -210.0), p(1723.0, 401.0), 1_010, true);
        assert_eq!(d, (-4.0, 1.0));
        assert!(!f.pending());
        assert_eq!(f.motion((-4.0, 0.0), p(1719.0, 401.0), 1_020, true), (-4.0, 0.0));
    }

    #[test]
    fn nudge_moves_toward_the_screen_interior() {
        let at = p(700.0, 0.0);
        let n = nudge_point(at, p(864.0, 558.5));
        assert!((n.x - at.x).hypot(n.y - at.y) - NUDGE_PT < 1e-9);
        assert!(n.y > at.y && n.x > at.x, "{n:?}");
        assert!(warp_moves_cursor(at, n) && warp_moves_cursor(n, at));
        assert_eq!(nudge_point(at, at), p(700.0 + NUDGE_PT, 0.0));
    }

    /// 大 warp 之后空闲 400 ms 再动：第一个事件仍然修正（F1），只修正一次。
    #[test]
    fn first_event_after_idle_is_still_corrected() {
        let mut f = WarpDeltaFix::new();
        f.warped(p(864.0, 559.0), 1_000_000_000);
        let d = f.motion((492.0, 99.0), p(868.0, 559.0), 1_400_000_000, true);
        assert_eq!(d, (4.0, 0.0));
        assert_eq!(f.motion((4.0, 0.0), p(872.0, 559.0), 1_016_000_000, true), (4.0, 0.0));
    }

    /// 实测（本机真实触摸板）：回到本机登记 warp 后的第一个事件，warp 还没生效——位移字段 (2, 1) 是
    /// 手指的一步，事件位置还是远程期间的旧值（进入点 + (−17, 99)）。要用位移字段，warp 留给下一个事件。
    #[test]
    fn event_before_the_warp_took_effect_keeps_the_delta_field() {
        let mut f = WarpDeltaFix::new();
        let entry = p(0.0, 770.0);
        f.warped(entry, 1_000);
        assert_eq!(f.motion((2.0, 1.0), p(-17.0, 869.0), 1_010, true), (2.0, 1.0));
        assert!(f.pending());
        // 下一个事件 warp 已生效：位移字段多出「进入点 − 旧位置」= (17, −99)。
        assert_eq!(f.motion((14.0, -99.0), p(-3.0, 770.0), 1_020, true), (-3.0, 0.0));
        assert!(!f.pending());
    }

    /// 选了位移字段之后，warp 最多再等 [`MAX_PENDING_EVENTS`] 个事件。
    #[test]
    fn pending_warp_expires() {
        let mut f = WarpDeltaFix::new();
        f.warped(p(0.0, 770.0), 1_000);
        assert_eq!(f.motion((2.0, 1.0), p(-40.0, 900.0), 1_010, true), (2.0, 1.0));
        assert_eq!(f.motion((2.0, 1.0), p(-40.0, 900.0), 1_020, true), (2.0, 1.0));
        assert!(!f.pending());
        // 光标回到进入点附近，也不再按位置算。
        assert_eq!(f.motion((5.0, 0.0), p(1.0, 770.0), 1_030, true), (5.0, 0.0));
    }

    /// 事件位置还冻结在进入点（F4 的停顿没去掉）：结果是 0，丢一步，不反向。
    #[test]
    fn frozen_location_after_warp_gives_zero_not_reverse() {
        let mut f = WarpDeltaFix::new();
        f.warped(p(1727.0, 400.0), 1_000);
        assert_eq!(f.motion((1500.0, -210.0), p(1727.0, 400.0), 1_010, true), (0.0, 0.0));
    }

    /// 时间戳早于 warp 的事件：原样使用，不消耗待修正的 warp。
    #[test]
    fn event_stamped_before_warp_does_not_consume_it() {
        let mut f = WarpDeltaFix::new();
        f.warped(p(1727.0, 400.0), 2_000);
        assert_eq!(f.motion((-6.0, 0.0), p(-280.0, 520.0), 1_990, true), (-6.0, 0.0));
        assert!(f.pending());
        assert_eq!(f.motion((1500.0, -210.0), p(1721.0, 400.0), 2_010, true), (-6.0, 0.0));
        assert!(!f.pending());
    }

    /// 回到本机的 warp 做完之前产生的事件由调用方吞掉；之后的照常修正。
    #[test]
    fn events_before_the_warp_completed_are_flagged() {
        let mut f = WarpDeltaFix::new();
        assert!(!f.predates_warp(10));
        f.warped(p(4.0, 744.0), 2_000);
        assert!(f.predates_warp(1_999));
        assert!(!f.predates_warp(2_000));
        assert!(!f.predates_warp(0), "unknown timestamps are not held back");
        assert_eq!(f.motion((-1515.0, 0.0), p(44.0, 744.0), 2_010, true), (40.0, 0.0));
        assert!(!f.predates_warp(1_999), "nothing pending any more");
    }

    /// 实测的坏情况：warp 之后第一个事件位置 1520、位移 +80，没有 warp——只记录。
    #[test]
    fn stale_first_event_is_recorded_once() {
        let mut f = WarpDeltaFix::new();
        f.warped(p(39.0, 744.0), 1_000);
        assert_eq!(f.motion((80.0, 0.0), p(1520.0, 744.0), 1_010, true), (80.0, 0.0));
        assert_eq!(f.take_mismatch(), Some(p(39.0, 744.0)));
        assert_eq!(f.take_mismatch(), None);
        assert_eq!(f.motion((40.0, 0.0), p(1560.0, 744.0), 1_020, true), (40.0, 0.0));
        assert_eq!(f.take_mismatch(), None);
        // 好的回到本机不记录。
        f.warped(p(39.0, 744.0), 2_000);
        f.motion((-1441.0, 0.0), p(78.0, 744.0), 2_010, true);
        assert_eq!(f.take_mismatch(), None);
    }

    /// 自己注入的移动事件重新确定了基准：之前的 warp 不再修正。
    #[test]
    fn posted_motion_clears_pending_warp() {
        let mut f = WarpDeltaFix::new();
        f.warped(p(864.0, 558.0), 100);
        // 早于这次 warp 的（在路上的）注入不算。
        f.posted(90);
        assert!(f.pending());
        f.posted(105);
        assert!(!f.pending());
        assert_eq!(f.motion((0.0, 5.0), p(300.0, 7.0), 110, true), (0.0, 5.0));
    }

    #[test]
    fn native_hid_position_is_the_base_for_resumed_physical_motion() {
        let mut f = WarpDeltaFix::new();
        f.motion((0.0, 0.0), p(640.0, 1000.0), 10, true);
        // Absolute HID input already advanced the OS's HID position before
        // this Mac became a source. The physical delta contains no program move.
        f.native_absolute_motion(p(2.0, 650.0), 20);
        f.enter_remote();
        assert_eq!(f.motion((0.0, 0.0), p(2.0, 651.0), 30, true), (0.0, 0.0));
        assert_eq!(f.motion((1.0, 2.0), p(3.0, 653.0), 40, true), (1.0, 2.0));
    }

    #[test]
    fn cg_relocation_after_native_hid_motion_counts_from_the_native_base() {
        let mut f = WarpDeltaFix::new();
        f.motion((0.0, 0.0), p(640.0, 1000.0), 10, true);
        f.posted_absolute_motion(p(300.0, 700.0), 20);
        f.native_absolute_motion(p(2.0, 650.0), 30);
        f.enter_remote();
        // A subsequent CG owner relocation still folds into physical motion,
        // but only the displacement after the native HID event belongs to it.
        f.posted_absolute_motion(p(30.0, 650.0), 40);
        assert_eq!(f.motion((29.0, 2.0), p(31.0, 652.0), 50, true), (1.0, 2.0));
        assert_eq!(f.take_leak(), None);
    }

    #[test]
    fn native_hid_motion_establishes_a_base_before_any_physical_input() {
        let mut f = WarpDeltaFix::new();
        f.posted_absolute_motion(p(100.0, 100.0), 10);
        f.native_absolute_motion(p(200.0, 200.0), 20);
        f.enter_remote();
        assert_eq!(f.motion((1.0, 2.0), p(201.0, 202.0), 30, true), (1.0, 2.0));
    }

    #[test]
    fn native_hid_motion_cannot_replace_a_newer_completed_local_warp() {
        let mut f = WarpDeltaFix::new();
        f.motion((0.0, 0.0), p(100.0, 100.0), 10, true);
        f.warped(p(200.0, 200.0), 30);
        f.native_absolute_motion(p(400.0, 175.0), 20);
        assert_eq!(f.motion((101.0, 102.0), p(201.0, 202.0), 40, true), (1.0, 2.0));
    }

    #[test]
    fn cg_absolute_motion_cannot_leak_into_first_local_step_sent_to_another_device() {
        let mut f = WarpDeltaFix::new();
        f.motion((0.0, 0.0), p(100.0, 100.0), 10, true);
        // Another device drives this Mac through several CG absolute moves.
        f.posted_absolute_motion(p(200.0, 150.0), 20);
        f.posted_absolute_motion(p(400.0, 175.0), 30);
        f.enter_remote(); // The pointer is now on the other device; no zone relocation.
        // macOS folds the injected displacement into the next hardware event.
        // Only the finger's (0, 1) step may be forwarded to the current holder.
        assert_eq!(f.motion((300.0, 76.0), p(400.0, 176.0), 40, true), (0.0, 1.0));
        assert_eq!(f.take_leak(), None);
        assert_eq!(f.motion((0.0, 2.0), p(400.0, 178.0), 50, true), (0.0, 2.0));
    }

    #[test]
    fn cg_absolute_motion_and_freeze_relocation_share_one_physical_base() {
        let mut f = WarpDeltaFix::new();
        f.motion((0.0, 0.0), p(100.0, 100.0), 10, true);
        f.posted_absolute_motion(p(400.0, 175.0), 20);
        f.enter_remote();
        assert!(f.posted_move(p(400.0, 30.0)));
        assert_eq!(f.motion((301.0, -68.0), p(401.0, 32.0), 30, true), (1.0, 2.0));
        assert_eq!(f.take_leak(), None);
    }

    #[test]
    fn cg_absolute_motion_before_local_warp_cannot_replace_its_repair() {
        let mut f = WarpDeltaFix::new();
        f.motion((0.0, 0.0), p(100.0, 100.0), 10, true);
        f.warped(p(200.0, 200.0), 30);
        f.posted_absolute_motion(p(400.0, 175.0), 20);
        assert_eq!(f.motion((101.0, 102.0), p(201.0, 202.0), 40, true), (1.0, 2.0));
        assert!(!f.pending());
    }

    #[test]
    fn cg_absolute_motion_rebases_after_physical_input_and_local_return() {
        let mut f = WarpDeltaFix::new();
        f.motion((0.0, 0.0), p(100.0, 100.0), 10, true);
        f.posted_absolute_motion(p(400.0, 175.0), 20);
        assert_eq!(f.motion((301.0, 77.0), p(401.0, 177.0), 30, true), (1.0, 2.0));
        f.posted_absolute_motion(p(500.0, 200.0), 40);
        f.enter_remote();
        assert_eq!(f.motion((98.0, 26.0), p(499.0, 203.0), 50, true), (-1.0, 3.0));
        f.posted_absolute_motion(p(600.0, 300.0), 60);
        f.warped(p(200.0, 200.0), 70); // Returning Local replaces the fold with warp repair.
        assert_eq!(f.motion((-298.0, -1.0), p(201.0, 202.0), 80, true), (1.0, 2.0));
    }

    /// 屏幕边缘夹住、取整、跨屏跳变的事件位置不影响没有 warp 时的位移（小数保留）。
    #[test]
    fn locations_never_steer_the_delta_without_a_warp() {
        let mut f = WarpDeltaFix::new();
        for (raw, at) in [
            ((8.0, 0.0), p(900.0, 1116.98)),
            ((-8.0, 0.0), p(900.0, 1116.98)),
            ((0.25, -0.75), p(864.0, 558.0)),
            ((0.25, -0.75), p(1864.0, 100.0)),
        ] {
            assert_eq!(f.motion(raw, at, 5, true), raw);
        }
    }

    // ---------------- 边缘停留结束时投递的移动 ----------------

    /// macOS 实测：冻结在底边 1116.98，投递的移动挪到 990，之后第一个位移字段
    /// (0, −120) = 手指 6 + (990 − 1116.98) 取整。按回调看到的顺序减去，只减一次。
    #[test]
    fn a_posted_freeze_move_is_subtracted_from_the_next_hardware_event() {
        let mut f = WarpDeltaFix::new();
        assert_eq!(f.motion((0.0, 6.0), p(864.0, 1116.98), 10, true), (0.0, 6.0));
        assert!(f.posted_move(p(864.0, 990.0)));
        assert!(f.pending());
        let d = f.motion((0.0, -120.98), p(864.0, 996.0), 20, true);
        assert!((d.0).abs() < 1e-9 && (d.1 - 6.0).abs() < 1e-9, "{d:?}");
        assert!(!f.pending());
        assert_eq!(f.motion((0.0, 6.0), p(864.0, 1002.0), 30, true), (0.0, 6.0));
    }

    /// 实测（macOS，左边缘）：停留时光标推到 x = 0，投递的移动挪到 3，下一步手指 −6 的事件位置被夹在
    /// 0（F8）。按位置算得 −3（错）；按「目标 − 之前的位置」减去得 −6。
    #[test]
    fn edge_clamped_location_does_not_matter() {
        let mut f = WarpDeltaFix::new();
        f.motion((-6.0, 0.0), p(0.0, 500.0), 10, true);
        f.posted_move(p(3.0, 500.0));
        assert_eq!(f.motion((-3.0, 0.0), p(0.0, 500.0), 20, true), (-6.0, 0.0));
    }

    /// 没见过任何 HID 事件时不知道系统从哪里算：丢掉下一个 HID 事件（只丢手指的一步），之后照常。
    #[test]
    fn a_posted_move_without_a_previous_hid_location_drops_the_next_hid_event() {
        let mut f = WarpDeltaFix::new();
        assert!(!f.posted_move(p(3.0, 500.0)));
        assert!(f.pending());
        // 别的程序投递的移动不受影响。
        assert_eq!(f.motion((2.0, 0.0), p(10.0, 10.0), 5, false), (2.0, 0.0));
        assert_eq!(f.motion((-800.0, 120.0), p(1.0, 500.0), 10, true), (0.0, 0.0));
        assert!(!f.pending());
        assert_eq!(f.motion((-2.0, 0.0), p(0.0, 500.0), 20, true), (-2.0, 0.0));
    }

    /// 实测（2026-09-29 本机被 Windows 控制之后）：最后一个触摸板事件在 (1028.2, 496.4)；之后是许多注入的移动
    /// （不是 HID 事件，不改变系统的起点）和一次 CGEventPost 的合成移动；冻结时投递到 (863.7, 30)；一分多钟后
    /// 第一个触摸板事件位移字段 (−173, −467)、位置 (854.9, 29.1)。应减去 (−164.5, −466.4)，发出手指的一步。
    #[test]
    fn the_fold_counts_from_the_last_hid_event_not_from_injected_moves() {
        let mut f = WarpDeltaFix::new();
        assert_eq!(f.motion((0.0, 0.0), p(1028.2, 496.4), 10, true), (0.0, 0.0));
        // 别的程序投递的移动：不是 HID 事件，原样使用，也不改变起点。
        assert_eq!(f.motion((3.0, 0.0), p(700.0, 300.0), 20, false), (3.0, 0.0));
        assert!(f.posted_move(p(863.7, 30.0)));
        let d = f.motion((-173.0, -467.0), p(854.94, 29.08), 30, true);
        assert!((d.0 + 8.5).abs() < 0.3 && (d.1 + 0.6).abs() < 0.3, "{d:?}");
        assert_eq!(f.take_leak(), None);
    }

    /// 按错误的起点减（旧代码）会留下检测记录：事件位置不是「目标 + 修正后的位移」。
    #[test]
    fn a_leaked_fold_is_reported() {
        let mut f = WarpDeltaFix::new();
        f.motion((0.0, 0.0), p(863.7, 0.0), 10, true);
        assert!(f.posted_move(p(863.7, 30.0)));
        // 系统其实从更早的 HID 位置 (1028.2, 496.4) 算（本测试模拟一个没记下的起点）：位移字段含 (−164.5, −466.4)。
        let d = f.motion((-173.0, -467.0), p(854.94, 29.08), 20, true);
        assert_eq!(d, (-173.0, -497.0));
        let leak = f.take_leak().expect("reported");
        assert_eq!(leak.target, p(863.7, 30.0));
    }

    fn mac() -> Vec<Rect> {
        vec![Rect::new(0.0, 0.0, 1728.0, 1117.0)]
    }

    #[test]
    fn outer_edges_of_a_single_display() {
        let d = mac();
        for q in [
            p(700.0, 1116.99),
            p(700.0, 0.0),
            p(0.0, 500.0),
            p(1727.99, 500.0),
            p(700.0, 1115.5),
        ] {
            assert!(at_outer_edge(q, &d), "{q:?}");
        }
        for q in [p(700.0, 1114.0), p(700.0, 3.0), p(864.0, 558.5)] {
            assert!(!at_outer_edge(q, &d), "{q:?}");
        }
    }

    #[test]
    fn clearing_moves_off_every_outer_edge_and_below_the_menu_bar() {
        let d = mac();
        // 底边（Dock）：挪到 3 pt 之外，沿边位置不变。
        assert_eq!(clear_of_edge_zones(p(700.0, 1116.99), &d), p(700.0, 1114.0));
        // 顶边（全屏菜单栏 29 pt）：挪到 30 pt。
        assert_eq!(clear_of_edge_zones(p(700.0, 0.0), &d), p(700.0, 30.0));
        assert_eq!(clear_of_edge_zones(p(700.0, 12.0), &d), p(700.0, 30.0));
        // 左右（侧边 Dock）。
        assert_eq!(clear_of_edge_zones(p(0.0, 500.0), &d), p(3.0, 500.0));
        assert_eq!(clear_of_edge_zones(p(1727.99, 500.0), &d), p(1725.0, 500.0));
        // 角：两个方向都挪开。
        assert_eq!(clear_of_edge_zones(p(1727.99, 1116.99), &d), p(1725.0, 1114.0));
        assert_eq!(clear_of_edge_zones(p(0.0, 0.0), &d), p(3.0, 30.0));
        // 不在感应区：不动（不会产生原地 warp）。
        for q in [p(700.0, 1114.0), p(700.0, 30.0), p(864.0, 558.5)] {
            assert_eq!(clear_of_edge_zones(q, &d), q);
        }
    }

    /// 两块屏幕上下排列（外接屏在上）：内屏的顶边、外接屏的底边在重叠的一段是内侧，不是外缘。
    #[test]
    fn inner_edges_between_local_displays_are_not_zones() {
        let d = vec![
            Rect::new(0.0, 0.0, 1728.0, 1117.0),
            Rect::new(200.0, -1080.0, 1920.0, 1080.0),
        ];
        // 内屏顶边 x = 700 上面是外接屏：不是外缘，不挪。
        assert!(!at_outer_edge(p(700.0, 0.0), &d));
        assert_eq!(clear_of_edge_zones(p(700.0, 0.0), &d), p(700.0, 0.0));
        // x = 100 上面没有屏幕：外缘（菜单栏只在最上面那块屏的顶边才有意义，但挪开无害）。
        assert!(at_outer_edge(p(100.0, 0.0), &d));
        assert_eq!(clear_of_edge_zones(p(100.0, 0.0), &d), p(100.0, 30.0));
        // 外接屏的底边在 x ≥ 1728 的一段是外缘。
        assert_eq!(clear_of_edge_zones(p(1800.0, -0.01), &d), p(1800.0, -3.0));
    }

    /// Dock 在底边、图标 87 pt：底边的冻结点离边 127 pt（离开弹出的 Dock），其他边照旧。
    #[test]
    fn clearance_with_the_dock_leaves_the_shown_dock() {
        let d = mac();
        let k = EdgeClearance::with_dock(DockSide::Bottom, 87.0);
        assert_eq!(
            clear_of_edge_zones_by(p(700.0, 1116.99), &d, k),
            p(700.0, 1117.0 - 127.0)
        );
        assert_eq!(clear_of_edge_zones_by(p(0.0, 500.0), &d, k), p(3.0, 500.0));
        assert_eq!(clear_of_edge_zones_by(p(700.0, 0.0), &d, k), p(700.0, 30.0));
        let k = EdgeClearance::with_dock(DockSide::Left, 48.0);
        assert_eq!(clear_of_edge_zones_by(p(0.0, 500.0), &d, k), p(88.0, 500.0));
        assert_eq!(clear_of_edge_zones_by(p(700.0, 1116.99), &d, k), p(700.0, 1114.0));
    }
}
