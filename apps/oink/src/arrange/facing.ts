import type { Rect } from './geometry';

type Axis = 'x' | 'y';
interface Projection {
    kind: 'pair' | 'visible';
    screen: number;
    side: number;
    from: number;
    to: number;
    target: number | null;
}
export const extent = (r: Rect, axis: Axis) => axis === 'x' ? r.w : r.h;
const perpendicular = (axis: Axis): Axis => axis === 'x' ? 'y' : 'x';
export const tolerance = (rects: Rect[]) => Math.max(1e-100, ...rects.flatMap(r => [r.w, r.h])) * 1e-10;

/** Geometric edge segments before optional routing deadzones or wall extension.
 * Coordinates are relative to each source edge; gap distance is deliberately absent. */
export function edgeProjections(rects: Rect[], eps = tolerance(rects)): Projection[] {
    const result: Projection[] = [];
    rects.forEach((a, i) => {
        for (let side = 0; side < 4; side++) {
            const axis: Axis = side < 2 ? 'x' : 'y';
            const along = perpendicular(axis);
            const lo = a[along], hi = lo + extent(a, along);
            const candidates = rects.flatMap((b, j) => {
                if (i === j) return [];
                const f = facing(a, b, side, eps);
                return f ? [{ target: j, ...f }] : [];
            });
            // Preserve every pair, including intervals currently hidden by other
            // computers. Nearest-visible intervals alone are not the full relation.
            for (const c of candidates) {
                result.push({ kind: 'pair', screen: i, side, from: c.from - lo, to: c.to - lo, target: c.target });
            }
            const cuts = [...new Set([lo, hi, ...candidates.flatMap(c => [c.from, c.to])])].sort((a, b) => a - b);
            let last: Projection | undefined;
            for (let cut = 1; cut < cuts.length; cut++) {
                const from = cuts[cut - 1], to = cuts[cut];
                if (to - from <= eps) continue;
                const mid = (from + to) / 2;
                const best = candidates.filter(c => c.from <= mid && mid <= c.to)
                    .sort((a, b) => a.gap - b.gap || a.target - b.target)[0];
                const target = best?.target ?? null;
                if (last && last.target === target) {
                    last.to = to - lo;
                } else {
                    last = { kind: 'visible', screen: i, side, from: from - lo, to: to - lo, target };
                    result.push(last);
                }
            }
        }
    });
    return result;
}

export function sameProjections(a: Projection[], b: Projection[], eps: number): boolean {
    return a.length === b.length && a.every((p, i) => {
        const q = b[i];
        return p.kind === q.kind && p.screen === q.screen && p.side === q.side && p.target === q.target
            && Math.abs(p.from - q.from) <= eps && Math.abs(p.to - q.to) <= eps;
    });
}

/** Mirrors oink-display/layout/facing.rs; both language suites check the shared conformance corpus.
 * Positive along-edge length, nonnegative gap up to scale-relative roundoff.
 * No raster rounding, corner-only contacts or UI pixel tolerance here. */
export function facing(a: Rect, b: Rect, side: number, eps = tolerance([a, b])) {
    const axis: Axis = side < 2 ? 'x' : 'y';
    const along = perpendicular(axis);
    const dir = side % 2 ? 1 : -1;
    const at = a[axis] + (dir > 0 ? extent(a, axis) : 0);
    const bt = b[axis] + (dir < 0 ? extent(b, axis) : 0);
    const gap = dir * (bt - at);
    const from = Math.max(a[along], b[along]);
    const to = Math.min(a[along] + extent(a, along), b[along] + extent(b, along));
    return gap >= -eps && to - from > eps ? { gap: Math.max(0, gap), from, to } : null;
}
export function overlaps(a: Rect, b: Rect, eps = tolerance([a, b])) {
    return Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x) > eps
        && Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y) > eps;
}

