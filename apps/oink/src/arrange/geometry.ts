import { facing, overlaps, tolerance } from './facing';
export { overlaps } from './facing';

export interface Rect {
    x: number;
    y: number;
    w: number;
    h: number;
}
export interface Screen extends Rect {
    id: string;
    name: string;
    wallpaper: number;
    labelWidth?: number;
    labelHeight?: number;
    minimumWidth?: number;
    image?: string;
    offline?: boolean;
    device?: string;
    groupSize?: number;
}
export type Corner = 'nw' | 'ne' | 'sw' | 'se';
export const CORNERS: Corner[] = ['nw', 'ne', 'sw', 'se'];
export const MIN_W = 640;
export const MAX_W = Infinity;
export interface Camera { zoom: number; ox: number; oy: number }
/** Fit the complete arrangement into the canvas insets, including enlargement. */
export function fitArrangement(fit: Rect, viewport: { w: number; h: number }, padding: number): Camera {
    const width = Math.max(0, viewport.w - 2 * padding);
    const height = Math.max(0, viewport.h - 56 - padding);
    const zoom = fit.w > 0 && fit.h > 0 && width > 0 && height > 0
        ? Math.min(width / fit.w, height / fit.h) : 0;
    return {
        zoom,
        ox: viewport.w / 2 - (fit.x + fit.w / 2) * zoom,
        oy: 56 + height / 2 - (fit.y + fit.h / 2) * zoom,
    };
}
export function overlapsAny(r: Rect, others: Rect[]): boolean {
    return others.some((o) => overlaps(r, o));
}
export function bounds(rects: Rect[]): Rect {
    if (!rects.length)
        return { x: 0, y: 0, w: 1920, h: 1080 };
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const r of rects) {
        x0 = Math.min(x0, r.x);
        y0 = Math.min(y0, r.y);
        x1 = Math.max(x1, r.x + r.w);
        y1 = Math.max(y1, r.y + r.h);
    }
    return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
}
export function clampMove(r: Rect, others: Rect[], axis: 'x' | 'y', d: number): number {
    const gap = facingGap(r, others, axis, d > 0 ? 1 : -1);
    return gap === null ? d : Math.sign(d) * Math.min(Math.abs(d), gap);
}
/** Matched moving edge / neighbour edge, including exact zero-distance matches. */
export interface SnapEdges { x?: string; y?: string }
export function snap(r: Rect, others: Rect[], threshold: number, edges?: SnapEdges): Rect {
    let bestX = threshold + 1, dx = 0;
    let bestY = threshold + 1, dy = 0;
    for (const [index, o] of others.entries()) {
        const nearY = r.y < o.y + o.h + threshold && o.y < r.y + r.h + threshold;
        const nearX = r.x < o.x + o.w + threshold && o.x < r.x + r.w + threshold;
        if (nearY) {
            for (const [edge, c] of [o.x - (r.x + r.w), o.x + o.w - r.x, o.x - r.x, o.x + o.w - (r.x + r.w)].entries()) {
                if (Math.abs(c) < bestX) {
                    bestX = Math.abs(c);
                    dx = c;
                    if (edges) edges.x = `${index}:${edge}`;
                }
            }
        }
        if (nearX) {
            for (const [edge, c] of [o.y - (r.y + r.h), o.y + o.h - r.y, o.y - r.y, o.y + o.h - (r.y + r.h)].entries()) {
                if (Math.abs(c) < bestY) {
                    bestY = Math.abs(c);
                    dy = c;
                    if (edges) edges.y = `${index}:${edge}`;
                }
            }
        }
    }
    if (edges && bestX > threshold) delete edges.x;
    if (edges && bestY > threshold) delete edges.y;
    return { ...r, x: r.x + (bestX <= threshold ? dx : 0), y: r.y + (bestY <= threshold ? dy : 0) };
}
export function cornerPoint(r: Rect, c: Corner) {
    return { x: c[1] === 'e' ? r.x + r.w : r.x, y: c[0] === 's' ? r.y + r.h : r.y };
}
export function cornerClearance(r: Rect, others: Rect[], eps = tolerance([r, ...others])): Record<Corner, number | null> {
    const r1 = { x: r.x + r.w, y: r.y + r.h };
    const edgeGap = (onLine: (o: Rect) => boolean, axis: 'x' | 'y', fromEnd: boolean): number | null => {
        const e0 = axis === 'x' ? r.x : r.y, e1 = axis === 'x' ? r1.x : r1.y;
        let best: number | null = null;
        for (const o of others) {
            if (!onLine(o))
                continue;
            const a = Math.max(e0, axis === 'x' ? o.x : o.y), b = Math.min(e1, axis === 'x' ? o.x + o.w : o.y + o.h);
            if (b < a - eps)
                continue;
            const g = Math.max(0, fromEnd ? e1 - b : a - e0);
            if (best === null || g < best)
                best = g;
        }
        return best;
    };
    const top = (o: Rect) => Math.abs(o.y + o.h - r.y) < eps;
    const bottom = (o: Rect) => Math.abs(o.y - r1.y) < eps;
    const left = (o: Rect) => Math.abs(o.x + o.w - r.x) < eps;
    const right = (o: Rect) => Math.abs(o.x - r1.x) < eps;
    const near = (h: number | null, v: number | null) => (h === null ? v : v === null ? h : Math.min(h, v));
    return {
        nw: near(edgeGap(top, 'x', false), edgeGap(left, 'y', false)),
        ne: near(edgeGap(top, 'x', true), edgeGap(right, 'y', false)),
        sw: near(edgeGap(bottom, 'x', false), edgeGap(left, 'y', true)),
        se: near(edgeGap(bottom, 'x', true), edgeGap(right, 'y', true)),
    };
}
export const opposite: Record<Corner, Corner> = { nw: 'se', ne: 'sw', sw: 'ne', se: 'nw' };
export function resizeFromCorner(origin: Rect, c: Corner, p: {
    x: number;
    y: number;
}, minimumWidth = MIN_W): Rect {
    const anchor = cornerPoint(origin, opposite[c]);
    const sx = c[1] === 'e' ? 1 : -1;
    const sy = c[0] === 's' ? 1 : -1;
    const dx = (p.x - anchor.x) * sx;
    const dy = (p.y - anchor.y) * sy;
    const { w: w0, h: h0 } = origin;
    let s = (dx * w0 + dy * h0) / (w0 * w0 + h0 * h0);
    s = Math.min(Math.max(s, minimumWidth / w0), MAX_W / w0);
    const w = w0 * s;
    const h = h0 * s;
    return { x: sx > 0 ? anchor.x : anchor.x - w, y: sy > 0 ? anchor.y : anchor.y - h, w, h };
}
/** Snap only the moving edges: translating a resize would move its fixed corner. */
export function snapResize(r: Rect, origin: Rect, c: Corner, others: Rect[], threshold: number, minimumWidth = MIN_W, edges?: SnapEdges): Rect {
    if (edges) { delete edges.x; delete edges.y; }
    const anchor = cornerPoint(origin, opposite[c]);
    const point = cornerPoint(r, c);
    const sx = c[1] === 'e' ? 1 : -1;
    const sy = c[0] === 's' ? 1 : -1;
    let best = Infinity;
    let result = r;
    const consider = (width: number, distance: number, axis: 'x' | 'y', edge: string) => {
        if (distance > threshold || width < minimumWidth || width > MAX_W || Math.abs(width - r.w) >= best)
            return;
        const h = width * origin.h / origin.w;
        best = Math.abs(width - r.w);
        if (edges) {
            delete edges.x;
            delete edges.y;
            edges[axis] = edge;
        }
        result = { x: sx > 0 ? anchor.x : anchor.x - width, y: sy > 0 ? anchor.y : anchor.y - h, w: width, h };
    };
    for (const [index, o] of others.entries()) {
        // Use the same neighbouring spans and canvas-pixel threshold as move snapping.
        const nearY = r.y < o.y + o.h + threshold && o.y < r.y + r.h + threshold;
        const nearX = r.x < o.x + o.w + threshold && o.x < r.x + r.w + threshold;
        if (nearY) {
            for (const [edge, x] of [o.x, o.x + o.w].entries())
                consider((x - anchor.x) * sx, Math.abs(x - point.x), 'x', `${index}:${edge}`);
        }
        if (nearX) {
            for (const [edge, y] of [o.y, o.y + o.h].entries())
                consider((y - anchor.y) * sy * origin.w / origin.h, Math.abs(y - point.y), 'y', `${index}:${edge}`);
        }
    }
    return result;
}
export function resizeAroundCenter(r: Rect, w: number, minimumWidth = MIN_W): Rect {
    const nw = Math.min(Math.max(w, minimumWidth), MAX_W);
    const nh = nw * r.h / r.w;
    return { x: r.x + (r.w - nw) / 2, y: r.y + (r.h - nh) / 2, w: nw, h: nh };
}
export function growAvoiding(r: Rect, w: number, others: Rect[], minimumWidth = MIN_W): Rect | null {
    const c = resizeAroundCenter(r, w, minimumWidth);
    const { w: nw, h: nh } = c;
    const anchors: Array<[
        number,
        number
    ]> = [[.5, .5], [.5, 0], [.5, 1], [0, .5], [1, .5], [0, 0], [1, 0], [0, 1], [1, 1]];
    for (const [ax, ay] of anchors) {
        const cand = { x: r.x + (r.w - nw) * ax, y: r.y + (r.h - nh) * ay, w: nw, h: nh };
        if (!overlapsAny(cand, others))
            return cand;
    }
    return null;
}
export function nearestFree(r: Rect, others: Rect[]): Rect {
    if (!overlapsAny(r, others))
        return r;
    const xs = [r.x, ...others.flatMap((o) => [o.x - r.w, o.x + o.w])];
    const ys = [r.y, ...others.flatMap((o) => [o.y - r.h, o.y + o.h])];
    let best: Rect | null = null;
    let bestD = Infinity;
    for (const x of xs) {
        for (const y of ys) {
            const d = (x - r.x) ** 2 + (y - r.y) ** 2;
            if (d >= bestD)
                continue;
            const cand = { ...r, x, y };
            if (!overlapsAny(cand, others)) {
                best = cand;
                bestD = d;
            }
        }
    }
    return best ?? r;
}
type Axis = 'x' | 'y';
export function facingGap(r: Rect, others: Rect[], axis: Axis, dir: 1 | -1): number | null {
    const side = (axis === 'y' ? 2 : 0) + (dir > 0 ? 1 : 0);
    const eps = tolerance([r, ...others]);
    const gaps = others.flatMap(o => {
        const f = facing(r, o, side, eps);
        return f ? [f.gap] : [];
    });
    return gaps.length ? Math.min(...gaps) : null;
}
export const sameRect = (a: Rect, b: Rect, eps = tolerance([a, b])) => Math.abs(a.x - b.x) < eps && Math.abs(a.y - b.y) < eps && Math.abs(a.w - b.w) < eps && Math.abs(a.h - b.h) < eps;
export const pickRect = ({ x, y, w, h }: Rect): Rect => ({ x, y, w, h });
