//! 传送门构建：把每块屏幕的每条边切分成「传送门」与「墙」。
//! 在逻辑布局上计算，只考虑参与的屏幕（启用且所属设备在线）。
//!
//! 对屏幕 A 的一条边：
//! 1. Candidates use the shared facing predicate (nonnegative gap up to arithmetic
//!    tolerance, no maximum distance), then require at least `minimum_portal` overlap.
//! 2. 每个子区间取间隙最小（最近）的候选，没有候选的部分是墙（大小屏并排时大屏多出的部分）；
//! 3. 吸附（消除测量噪声）：边两端、或两个传送门之间短于 `portal_snap` 的墙并入传送门（中间的从中点劈开）；
//! 4. 关闭墙（`walls = false`）时：有传送门的边上所有墙都分给最近的传送门；
//! 5. Like visual projections, nearer screens occlude only their covered intervals.
//!    A visible but ineligible neighbour (for example, too little overlap) is a
//!    wall on that interval; it cannot erase a farther neighbour elsewhere.
//!    Occluded intervals cannot be reopened by wall snapping or endpoint reach.

use super::{
    EffectiveSettings, ScreenId, Side,
    facing::{facing, tolerance},
};
use oink_foundation::Rect;
use serde::{Deserialize, Serialize};

/// 一条边上的一段（沿边坐标，plane：左右边为 y，上下边为 x）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub from: f64,
    pub to: f64,
    pub kind: SegmentKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SegmentKind {
    /// 墙（死区）：指针沿边滑动。
    Wall,
    /// 传送门：穿过后进入 `to` 的相对边，沿边位置不变（stitching，超出目标边范围的部分贴到端点），
    /// 跳过 `gap_distance` 的间隙（逻辑布局里相接的屏幕为 0；中间隔着离线设备的屏幕时是它的宽度）。
    Portal { to: ScreenId, gap_distance: f64 },
}

/// 一条边的切分结果（给界面绘制）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EdgeInfo {
    pub screen: ScreenId,
    pub side: Side,
    /// 边所在的位置（plane）：左右边为 x，上下边为 y。
    pub at: f64,
    pub segments: Vec<Segment>,
}

/// 内部表示：目标用下标。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Seg {
    pub from: f64,
    pub to: f64,
    /// `(目标屏下标, 间隙)`；`None` 为墙。
    pub portal: Option<(usize, f64)>,
    /// A nearer screen occupies this wall interval; snapping must not bypass it.
    pub blocked: bool,
}

impl Seg {
    fn len(&self) -> f64 {
        self.to - self.from
    }
    fn same_kind(&self, o: &Seg) -> bool {
        match (self.portal, o.portal) {
            (None, None) => self.blocked == o.blocked,
            (Some((a, _)), Some((b, _))) => a == b,
            _ => false,
        }
    }
}

/// 边的位置与沿边范围：`(at, lo, hi)`。
pub(crate) fn edge(r: &Rect, side: Side) -> (f64, f64, f64) {
    match side {
        Side::Left => (r.x, r.y, r.bottom()),
        Side::Right => (r.right(), r.y, r.bottom()),
        Side::Top => (r.y, r.x, r.right()),
        Side::Bottom => (r.bottom(), r.x, r.right()),
    }
}

/// 计算屏幕 `a` 的 `side` 边的切分。`active[i]` 为 `false` 的屏幕不参与。
pub(crate) fn compute_side(
    rects: &[Rect],
    active: &[bool],
    a: usize,
    side: Side,
    st: &EffectiveSettings,
    minimum_overlap: impl Fn(usize) -> Option<f64>,
) -> Vec<Seg> {
    let (_, a0, a1) = edge(&rects[a], side);
    let eps = tolerance(rects);
    // Candidate: (index, gap, overlap start/end, entry eligible).
    let mut cands: Vec<(usize, f64, f64, f64, bool)> = Vec::new();
    for (b, rb) in rects.iter().enumerate() {
        if b == a || !active[b] {
            continue;
        }
        let Some(f) = facing(rects[a], *rb, side, eps) else {
            continue;
        };
        let eligible = minimum_overlap(b).is_some_and(|minimum| f.to - f.from >= minimum.max(0.0));
        // Include every facing screen when selecting nearest visibility, even
        // when that screen cannot itself be entered. Its occlusion is local.
        cands.push((b, f.gap, f.from, f.to, eligible));
    }

    let mut cuts: Vec<f64> = vec![a0, a1];
    for c in &cands {
        cuts.push(c.2);
        cuts.push(c.3);
    }
    cuts.sort_by(f64::total_cmp);
    cuts.dedup();

    let mut segs: Vec<Seg> = Vec::new();
    for w in cuts.windows(2) {
        let (u, v) = (w[0], w[1]);
        if v - u <= eps {
            continue;
        }
        let mid = (u + v) / 2.0;
        let best = cands
            .iter()
            .filter(|c| c.2 <= mid && mid <= c.3)
            .min_by(|x, y| x.1.total_cmp(&y.1).then(x.0.cmp(&y.0)));
        let seg = Seg {
            from: u,
            to: v,
            portal: best.filter(|c| c.4).map(|c| (c.0, c.1)),
            blocked: best.is_some_and(|c| !c.4),
        };
        match segs.last_mut() {
            Some(last) if last.same_kind(&seg) => last.to = v,
            _ => segs.push(seg),
        }
    }
    if segs.is_empty() {
        return vec![Seg {
            from: a0,
            to: a1,
            portal: None,
            blocked: false,
        }];
    }
    refine(&mut segs, st);
    segs
}

/// 吸附、关闭墙。
fn refine(segs: &mut Vec<Seg>, st: &EffectiveSettings) {
    if !segs.iter().any(|s| s.portal.is_some()) {
        return;
    }
    let absorb_end = |len: f64| !st.walls || len < st.portal_snap;
    let bridge = absorb_end;

    // 两端的墙。
    if segs.len() >= 2 && segs[0].portal.is_none() && !segs[0].blocked && absorb_end(segs[0].len()) {
        segs[1].from = segs[0].from;
        segs.remove(0);
    }
    let n = segs.len();
    if n >= 2 && segs[n - 1].portal.is_none() && !segs[n - 1].blocked && absorb_end(segs[n - 1].len()) {
        segs[n - 2].to = segs[n - 1].to;
        segs.pop();
    }
    // 中间的墙：两侧都是传送门时从中点劈开。
    let mut i = 1;
    while i + 1 < segs.len() {
        if segs[i].portal.is_none()
            && !segs[i].blocked
            && segs[i - 1].portal.is_some()
            && segs[i + 1].portal.is_some()
            && bridge(segs[i].len())
        {
            let mid = (segs[i].from + segs[i].to) / 2.0;
            segs[i - 1].to = mid;
            segs[i + 1].from = mid;
            segs.remove(i);
        } else {
            i += 1;
        }
    }
    // 合并相邻的同一目标。
    let mut j = 1;
    while j < segs.len() {
        if segs[j - 1].same_kind(&segs[j]) {
            segs[j - 1].to = segs[j].to;
            segs.remove(j);
        } else {
            j += 1;
        }
    }
}
