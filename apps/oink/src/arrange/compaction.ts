import type { Rect } from './geometry';
import { bounds } from './geometry';
import { edgeProjections, facing, overlaps, sameProjections, tolerance } from './facing';

export interface Offset { dx: number; dy: number }
const NONE: Offset = { dx: 0, dy: 0 };

/**
 * Auto-compaction after a manual edit (user decisions 2026-10-21).
 *
 * Only the edited computer moves, as one rigid translation; untouched computers
 * never move. `moving` holds the edited computer's visible screens and `others`
 * the visible screens of every other computer, keyed by computer in `groups`.
 * Hidden or disabled screens are left out by the caller: they are not part of
 * bounding rectangles, overlap checks or projections.
 *
 * No screen's projection relationship with any other screen may change: every
 * directed facing pair and its source-relative interval stays as it was, and
 * only gap distances shrink. Overlap is never allowed. Within those rules the
 * computer closes as much empty space as it can. A move must end in contact,
 * either with a real screen or with the bounding rectangle of another computer.
 * The rectangle is the stopping place when reaching a real screen would change
 * a projection; a computer with no overlap on either axis meets it corner to
 * corner. The smallest such translation wins, so the nearest computer is chosen.
 * Moves repeat while each one keeps every earlier contact and adds a new one.
 * Relationships are compared between the released and the final layout; a move
 * may pass a screen's side on the way but never through a screen.
 */
export function settleGroup(moving: Rect[], others: Rect[], groups: string[]): Offset {
    if (!moving.length || !others.length) return NONE;
    const eps = tolerance([...moving, ...others]);
    const boxes = [...new Set(groups)].map(g => bounds(others.filter((_, i) => groups[i] === g)));
    const at = (o: Offset) => moving.map(r => ({ ...r, x: r.x + o.dx, y: r.y + o.dy }));
    const before = edgeProjections([...moving, ...others], eps);
    /** The facing pairs between the edited computer and every other screen, as source-
     * relative intervals: the only pairs a translation can change. Comparing these first
     * rejects most candidates in O(moving × others) before the full projection set, which
     * also covers which interval is visible, is rebuilt. */
    const pairs = (placed: Rect[]) => placed.flatMap(m => others.flatMap(r => [0, 1, 2, 3].flatMap(side => {
        const lo = (a: Rect) => side < 2 ? a.y : a.x, f = facing(m, r, side, eps), g = facing(r, m, side, eps);
        return [f ? 1 : 0, f ? f.from - lo(m) : 0, f ? f.to - lo(m) : 0, g ? 1 : 0, g ? g.from - lo(r) : 0, g ? g.to - lo(r) : 0];
    })));
    const pairsBefore = pairs(moving);
    const valid = (o: Offset) => {
        const placed = at(o);
        return placed.every(m => others.every(r => !overlaps(m, r, eps)))
            && pairs(placed).every((v, k) => Math.abs(v - pairsBefore[k]) <= eps)
            && sameProjections(before, edgeProjections([...placed, ...others], eps), eps);
    };
    /** Real screens it shares an edge with, and bounding rectangles it touches, corners included. */
    const touches = (o: Offset) => {
        const placed = at(o), keys = new Set<string>();
        others.forEach((r, j) => { if (placed.some(m => edgeContact(m, r, eps))) keys.add(`s${j}`); });
        boxes.forEach((r, j) => { if (placed.some(m => touching(m, r, eps))) keys.add(`b${j}`); });
        return keys;
    };
    /** Whether a move keeps every contact in `kept` and adds one, i.e. `touches(o)` is a
     * strict superset of `kept`. Equivalent to comparing the sets, but returns at the
     * first lost or new contact: almost every candidate translation fails one of the two. */
    const gains = (o: Offset, kept: Set<string>) => {
        const placed = at(o);
        const holds = (key: string) => {
            const j = Number(key.slice(1));
            return key[0] === 's' ? placed.some(m => edgeContact(m, others[j], eps)) : placed.some(m => touching(m, boxes[j], eps));
        };
        for (const key of kept) if (!holds(key)) return false;
        return others.some((_, j) => !kept.has(`s${j}`) && holds(`s${j}`)) || boxes.some((_, j) => !kept.has(`b${j}`) && holds(`b${j}`));
    };

    const targets = [...others, ...boxes];
    let offset = NONE;
    for (let step = 0; step < targets.length; step++) {
        const kept = touches(offset), placed = at(offset);
        // Every contact puts a group edge on a target edge on one axis and at most a
        // corner apart on the other, so these per-axis translations cover them all.
        const shifts = (axis: 'x' | 'y') => {
            const size = axis === 'x' ? 'w' : 'h';
            // Aligned screens repeat the same translation; test each value once, first occurrence kept.
            return [...new Set([0, ...placed.flatMap(m => targets.flatMap(t => [t[axis] + t[size] - m[axis], t[axis] - (m[axis] + m[size])]))])];
        };
        let best: Offset | undefined, cost = Infinity;
        for (const dx of shifts('x'))
            for (const dy of shifts('y')) {
                const d = Math.hypot(dx, dy);
                if (d <= eps || d >= cost - eps) continue;
                const reached = { dx: offset.dx + dx, dy: offset.dy + dy };
                if (gains(reached, kept) && valid(reached)
                    && pathFree(moving, others, offset, reached, eps)) {
                    best = reached;
                    cost = d;
                }
            }
        if (!best) break;
        offset = best;
    }
    return Math.abs(offset.dx) <= eps && Math.abs(offset.dy) <= eps ? NONE : offset;
}

/** One computer's visible screens; hidden or disabled screens are left out by the caller. */
export interface Computer { device: string; rects: Rect[] }

/**
 * The single pass run when auto-compact is turned on (user decision 2026-10-21).
 *
 * There is no edited computer, so `anchor` (this device) stays fixed and every other
 * computer gets one `settleGroup` move per pass, with all of its rules, nearest to the
 * anchor first (bounding-rectangle distance, ties by device ID). Passes repeat while
 * something moves, at most once per computer; each move only closes gaps, so this is a
 * bound, not a cutoff. Without a visible anchor the order is by device ID alone.
 * Returns each moved computer's total translation and, per pass, who moved.
 */
export function compactAround(computers: Computer[], anchor: string | null): { offsets: Map<string, Offset>; moves: string[][] } {
    const offsets = new Map<string, Offset>(), moves: string[][] = [];
    const placed = (c: Computer) => {
        const o = offsets.get(c.device) ?? NONE;
        return c.rects.map(r => ({ ...r, x: r.x + o.dx, y: r.y + o.dy }));
    };
    const fixed = computers.find(c => c.device === anchor && c.rects.length);
    const distance = (c: Computer) => {
        if (!fixed) return 0;
        const a = bounds(placed(fixed)), b = bounds(placed(c));
        return Math.hypot(Math.max(0, b.x - (a.x + a.w), a.x - (b.x + b.w)), Math.max(0, b.y - (a.y + a.h), a.y - (b.y + b.h)));
    };
    const movable = computers.filter(c => c.device !== anchor && c.rects.length);
    for (let pass = 0; pass < computers.length; pass++) {
        const order = movable.map(c => ({ c, d: distance(c) }))
            .sort((x, y) => x.d - y.d || (x.c.device < y.c.device ? -1 : x.c.device > y.c.device ? 1 : 0)).map(x => x.c);
        const moved: string[] = [];
        for (const c of order) {
            const rest = computers.filter(o => o !== c);
            const others = rest.flatMap(placed), groups = rest.flatMap(o => o.rects.map(() => o.device));
            const { dx, dy } = settleGroup(placed(c), others, groups);
            if (dx === 0 && dy === 0) continue;
            const o = offsets.get(c.device) ?? NONE;
            offsets.set(c.device, { dx: o.dx + dx, dy: o.dy + dy });
            moved.push(c.device);
        }
        if (!moved.length) break;
        moves.push(moved);
    }
    return { offsets, moves };
}

/** A shared edge with a positive span; a corner alone does not join two screens. */
const edgeContact = (a: Rect, b: Rect, eps: number) =>
    [0, 1, 2, 3].some(side => (facing(a, b, side, eps)?.gap ?? Infinity) <= eps);

/** Edge or corner contact: no gap on either axis, and no overlap. */
const touching = (a: Rect, b: Rect, eps: number) =>
    !overlaps(a, b, eps)
    && Math.max(b.x - (a.x + a.w), a.x - (b.x + b.w)) <= eps
    && Math.max(b.y - (a.y + a.h), a.y - (b.y + b.h)) <= eps;

/** Overlap can begin only where an edge passes another edge, so check every such
 * coincidence and each open interval between them: a move never passes through a
 * screen. Projections are compared at the end, against the released layout. */
function pathFree(moving: Rect[], others: Rect[], from: Offset, to: Offset, eps: number): boolean {
    const delta = { x: to.dx - from.dx, y: to.dy - from.dy };
    const events = [0, 1];
    for (const axis of ['x', 'y'] as const) {
        if (Math.abs(delta[axis]) <= eps) continue;
        const size = axis === 'x' ? 'w' : 'h', shift = axis === 'x' ? from.dx : from.dy;
        for (const m of moving)
            for (const r of others)
                for (const p of [m[axis] + shift, m[axis] + m[size] + shift])
                    for (const q of [r[axis], r[axis] + r[size]]) {
                        const s = (q - p) / delta[axis];
                        if (s > 0 && s < 1) events.push(s);
                    }
    }
    events.sort((a, b) => a - b);
    const free = (s: number) => moving.every(m => others.every(r => !overlaps(
        { ...m, x: m.x + from.dx + delta.x * s, y: m.y + from.dy + delta.y * s }, r, eps)));
    return events.slice(1).every((s, i) => free((events[i] + s) / 2) && free(s));
}
