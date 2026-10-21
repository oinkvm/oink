import {
    bounds,
    overlapsAny,
    growAvoiding,
    resizeAroundCenter,
    snapResize,
    cornerPoint,
    opposite,
    type Corner,
    type Rect,
    type Screen,
    type SnapEdges,
} from './geometry';
import { tolerance } from './facing';

/** Missing device IDs describe independent screens in standalone editor fixtures. */
export const sameComputer = (a: Screen, b: Screen) => a.id === b.id || !!a.device && a.device === b.device;

export const membersOf = (screens: Screen[], id: string) => {
    const screen = screens.find(s => s.id === id);
    return screen ? screens.filter(s => sameComputer(screen, s)) : [];
};

export function groupScreen(screens: Screen[], id: string): Screen | undefined {
    const members = membersOf(screens, id);
    if (!members.length)
        return undefined;
    const r = bounds(members);
    return {
        ...members.find(s => s.id === id)!,
        ...r,
        minimumWidth: r.w * Math.max(...members.map(s => (s.minimumWidth ?? 640) / s.w)),
    };
}

export function transformGroup<T extends Rect>(members: T[], from: Rect, to: Rect): T[] {
    const k = to.w / from.w;
    return members.map(s => ({
        ...s,
        x: to.x + (s.x - from.x) * k,
        y: to.y + (s.y - from.y) * k,
        w: s.w * k,
        h: s.h * k,
    }));
}

export function groupOverlaps(members: Rect[], from: Rect, to: Rect, others: Rect[]): boolean {
    return transformGroup(members, from, to).some(s => overlapsAny(s, others));
}

/** Resolve actual screen collisions, allowing another computer in a desktop gap. */
export function freeGroup(members: Rect[], from: Rect, to: Rect, others: Rect[]): Rect {
    const live = transformGroup(members, from, to);
    if (!live.some(s => overlapsAny(s, others)))
        return to;
    const xs = [0], ys = [0];
    for (const s of live)
        for (const o of others) {
            xs.push(o.x + o.w - s.x, o.x - s.x - s.w);
            ys.push(o.y + o.h - s.y, o.y - s.y - s.h);
        }
    let best: Rect | undefined, cost = Infinity;
    for (const dx of xs)
        for (const dy of ys) {
            const d = dx * dx + dy * dy;
            if (d >= cost)
                continue;
            const next = { ...to, x: to.x + dx, y: to.y + dy };
            if (!groupOverlaps(members, from, next, others)) {
                best = next;
                cost = d;
            }
        }
    return best ?? from;
}

interface OuterEdge {
    axis: 'x' | 'y';
    at: number;
    from: number;
    to: number;
    key: string;
}

/** Remove edge intervals covered by another screen of this native desktop. */
function outerEdges(rects: Rect[]): OuterEdge[] {
    const edges: OuterEdge[] = [];
    const eps = tolerance(rects);
    for (const [i, r] of rects.entries())
        for (const side of ['left', 'right', 'top', 'bottom'] as const) {
            const axis = side === 'left' || side === 'right' ? 'x' : 'y';
            const at = side === 'left' ? r.x
                : side === 'right' ? r.x + r.w
                : side === 'top' ? r.y : r.y + r.h;
            let spans = axis === 'x' ? [[r.y, r.y + r.h]] : [[r.x, r.x + r.w]];
            for (const [j, o] of rects.entries()) {
                if (i === j)
                    continue;
                const covers = side === 'left' ? o.x < at - eps && o.x + o.w >= at - eps
                    : side === 'right' ? o.x <= at + eps && o.x + o.w > at + eps
                    : side === 'top' ? o.y < at - eps && o.y + o.h >= at - eps
                    : o.y <= at + eps && o.y + o.h > at + eps;
                if (!covers)
                    continue;
                const lo = axis === 'x' ? o.y : o.x, hi = lo + (axis === 'x' ? o.h : o.w);
                spans = spans.flatMap(([a, b]) => hi <= a || lo >= b
                    ? [[a, b]]
                    : [[a, Math.min(b, lo)], [Math.max(a, hi), b]].filter(([a, b]) => b > a));
            }
            for (const [from, to] of spans)
                edges.push({ axis, at, from, to, key: `${i}:${side}` });
        }
    return edges;
}

function matches(edge: OuterEdge, others: Rect[], threshold: number): Array<{
    at: number;
    key: string;
}> {
    return others.flatMap((o, i) => {
        const lo = edge.axis === 'x' ? o.y : o.x, hi = lo + (edge.axis === 'x' ? o.h : o.w);
        if (edge.from >= hi + threshold || lo >= edge.to + threshold)
            return [];
        const positions = edge.axis === 'x' ? [o.x, o.x + o.w] : [o.y, o.y + o.h];
        return positions.flatMap((at, side) => Math.abs(at - edge.at) <= threshold
            ? [{ at, key: `${edge.key}:${i}:${side}` }] : []);
    });
}

/** Snap exposed screen edges, translating the complete rigid desktop. */
export function snapGroup(
    members: Rect[],
    from: Rect,
    to: Rect,
    others: Rect[],
    threshold: number,
    edges: SnapEdges,
): Rect {
    let dx = Infinity, dy = Infinity;
    for (const edge of outerEdges(transformGroup(members, from, to)))
        for (const match of matches(edge, others, threshold)) {
            const delta = match.at - edge.at;
            if (edge.axis === 'x' && Math.abs(delta) < Math.abs(dx)) {
                dx = delta;
                edges.x = match.key;
            }
            if (edge.axis === 'y' && Math.abs(delta) < Math.abs(dy)) {
                dy = delta;
                edges.y = match.key;
            }
        }
    return {
        ...to,
        x: to.x + (Number.isFinite(dx) ? dx : 0),
        y: to.y + (Number.isFinite(dy) ? dy : 0),
    };
}

/** Resizing fixes the opposite group corner and snaps moving exposed edges. */
export function snapGroupResize(
    members: Rect[],
    from: Rect,
    to: Rect,
    c: Corner,
    others: Rect[],
    threshold: number,
    minimumWidth: number,
    edges: SnapEdges,
): Rect {
    if (members.length === 1)
        return snapResize(to, from, c, others, threshold, minimumWidth, edges);
    const anchor = cornerPoint(from, opposite[c]);
    let result = to, best = Infinity;
    for (const edge of outerEdges(transformGroup(members, from, to)))
        for (const match of matches(edge, others, threshold)) {
            const distance = (edge.at - anchor[edge.axis]) / (to.w / from.w);
            if (Math.abs(distance) < 1e-9)
                continue;
            const k = (match.at - anchor[edge.axis]) / distance, width = from.w * k;
            if (width < minimumWidth || Math.abs(width - to.w) >= best)
                continue;
            result = {
                x: anchor.x + (from.x - anchor.x) * k,
                y: anchor.y + (from.y - anchor.y) * k,
                w: width,
                h: from.h * k,
            };
            best = Math.abs(width - to.w);
            delete edges.x;
            delete edges.y;
            edges[edge.axis] = match.key;
        }
    return result;
}

/** Carry a visible gesture through hidden members before recording history. */
export function mergeVisibleGroups(all: Screen[], visible: Screen[]): Screen[] {
    let result = all;
    const devices = new Set(visible.map(s => s.device ?? s.id));
    for (const device of devices) {
        const next = visible.find(n =>
            (n.device ?? n.id) === device
            && all.some(old => old.id === n.id && (old.x !== n.x || old.y !== n.y || old.w !== n.w)),
        );
        if (!next)
            continue;
        const old = all.find(s => s.id === next.id)!;
        const transformed = transformGroup(membersOf(all, old.id), old, next);
        result = result.map(s => transformed.find(n => n.id === s.id) ?? s);
    }
    return result;
}

/** Keyboard growth tests actual group screens instead of filling desktop gaps. */
export function growGroupAvoiding(
    members: Rect[],
    from: Rect,
    width: number,
    others: Rect[],
    minimumWidth = 640,
): Rect | null {
    if (members.length === 1)
        return growAvoiding(from, width, others, minimumWidth);
    const sized = resizeAroundCenter(from, width, minimumWidth);
    const k = sized.w / from.w;
    for (const [ax, ay] of [
        [.5, .5], [.5, 0], [.5, 1], [0, .5], [1, .5],
        [0, 0], [1, 0], [0, 1], [1, 1],
    ]) {
        const target = {
            x: from.x + (from.w - sized.w) * ax,
            y: from.y + (from.h - from.h * k) * ay,
            w: sized.w,
            h: from.h * k,
        };
        if (!groupOverlaps(members, from, target, others))
            return target;
    }
    return null;
}
