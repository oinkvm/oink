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
    const valid = (o: Offset) => {
        const placed = at(o);
        return placed.every(m => others.every(r => !overlaps(m, r, eps)))
            && sameProjections(before, edgeProjections([...placed, ...others], eps), eps);
    };
    /** Real screens it shares an edge with, and bounding rectangles it touches, corners included. */
    const touches = (o: Offset) => {
        const placed = at(o), keys = new Set<string>();
        others.forEach((r, j) => { if (placed.some(m => edgeContact(m, r, eps))) keys.add(`s${j}`); });
        boxes.forEach((r, j) => { if (placed.some(m => touching(m, r, eps))) keys.add(`b${j}`); });
        return keys;
    };

    const targets = [...others, ...boxes];
    let offset = NONE;
    for (let step = 0; step < targets.length; step++) {
        const kept = touches(offset), placed = at(offset);
        // Every contact puts a group edge on a target edge on one axis and at most a
        // corner apart on the other, so these per-axis translations cover them all.
        const shifts = (axis: 'x' | 'y') => {
            const size = axis === 'x' ? 'w' : 'h';
            return [0, ...placed.flatMap(m => targets.flatMap(t => [t[axis] + t[size] - m[axis], t[axis] - (m[axis] + m[size])]))];
        };
        let best: Offset | undefined, cost = Infinity;
        for (const dx of shifts('x'))
            for (const dy of shifts('y')) {
                const d = Math.hypot(dx, dy);
                if (d <= eps || d >= cost - eps) continue;
                const reached = { dx: offset.dx + dx, dy: offset.dy + dy };
                const now = touches(reached);
                if (now.size > kept.size && [...kept].every(k => now.has(k)) && valid(reached)
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
