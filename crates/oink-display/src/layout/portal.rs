//! 传送门构建：把每块屏幕的每条边切分成「传送门」与「墙」。
//! 在逻辑布局上计算，只考虑参与的屏幕（启用且所属设备在线）。
//!
//! 对屏幕 A 的一条边：
//! 1. Candidates use the shared facing predicate (nonnegative gap up to arithmetic
//!    tolerance, no maximum distance), then require at least `minimum_portal` overlap.
//! 2. 每个子区间取间隙最小（最近）的候选，没有候选的部分是墙（大小屏并排时大屏多出的部分）；
//! 3. 吸附（消除测量噪声）：边两端、或两个传送门之间短于 `portal_snap` 的墙并入传送门（中间的从中点劈开）；
//! 4. 关闭墙（`walls = false`）时：有传送门的边上所有墙都分给最近的传送门；
//! 5. 不越过参与的屏幕（第 1 步的候选里先去掉）：A 与候选 B 之间隔着另一块参与的屏幕 C 时，B 不是候选——
//!    C 在法线方向上位于 A、B 的间隙里，且 C 沿边的范围与 A、B 相对的范围重叠超过 `overlap_tolerance`。
//!    例如左屏比中屏高出一截，高出的那段不越过中屏通到右屏，而是墙（再按吸附、关闭墙处理）。离线或未启用的
//!    屏幕不参与，也不挡路。

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
}

impl Seg {
    fn len(&self) -> f64 {
        self.to - self.from
    }
    fn same_kind(&self, o: &Seg) -> bool {
        match (self.portal, o.portal) {
            (None, None) => true,
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
    let (at, a0, a1) = edge(&rects[a], side);
    let eps = tolerance(rects);
    // 候选：(下标, 间隙, 重叠起点, 重叠终点)
    let mut cands: Vec<(usize, f64, f64, f64)> = Vec::new();
    for (b, rb) in rects.iter().enumerate() {
        if b == a || !active[b] {
            continue;
        }
        let Some(minimum_overlap) = minimum_overlap(b) else {
            continue;
        };
        let Some(f) = facing(rects[a], *rb, side, eps) else {
            continue;
        };
        if f.to - f.from < minimum_overlap.max(0.0) {
            continue;
        }
        cands.push((b, f.gap, f.from, f.to));
    }
    // 不越过参与的屏幕：A 与 B 之间隔着 C（法线方向在间隙里，沿边与 A、B 相对的范围重叠）时去掉 B。
    let tol = st.overlap_tolerance.max(0.0);
    let far = |gap: f64| match side {
        Side::Right | Side::Bottom => at + gap,
        Side::Left | Side::Top => at - gap,
    };
    cands.retain(|&(b, gap, lo, hi)| {
        let (g0, g1) = {
            let (p, q) = (at, far(gap));
            (p.min(q), p.max(q))
        };
        !rects.iter().enumerate().any(|(c, rc)| {
            if c == a || c == b || !active[c] {
                return false;
            }
            // 法线方向（左右边为 x，上下边为 y）与沿边方向的范围
            let ((c0, c1), (d0, d1)) = if side.is_vertical() {
                ((rc.x, rc.right()), (rc.y, rc.bottom()))
            } else {
                ((rc.y, rc.bottom()), (rc.x, rc.right()))
            };
            let between = c1.min(g1) - c0.max(g0) > eps;
            let along = d1.min(hi) - d0.max(lo) > tol.max(eps);
            between && along
        })
    });

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
            portal: best.map(|c| (c.0, c.1)),
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
    if segs.len() >= 2 && segs[0].portal.is_none() && absorb_end(segs[0].len()) {
        segs[1].from = segs[0].from;
        segs.remove(0);
    }
    let n = segs.len();
    if n >= 2 && segs[n - 1].portal.is_none() && absorb_end(segs[n - 1].len()) {
        segs[n - 2].to = segs[n - 1].to;
        segs.pop();
    }
    // 中间的墙：两侧都是传送门时从中点劈开。
    let mut i = 1;
    while i + 1 < segs.len() {
        if segs[i].portal.is_none()
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
