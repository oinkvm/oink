import type { Rect, Screen } from './geometry';
import { bounds } from './geometry';

export interface Offset { dx: number; dy: number }
export interface Unit { device: string; rect: Rect }
const NONE: Offset = { dx: 0, dy: 0 };
// 2^-30 layout units, in the renderer's 4096x coordinate plane.
const SCALE = 2 ** 18;
const fixed = (v: number) => BigInt(Math.floor(v * SCALE + .5));
// Optimize on the fixed grid, but retain the original coordinate of every
// endpoint. Applying a rounded displacement to an unrounded origin leaves a
// gap/overlap larger than contact roundoff; the selected edge must be reused.
interface Coordinate { grid: bigint; value: number }
const coordinate = (value: number, grid = fixed(value)): Coordinate => ({ grid, value });
interface Axis {
    start: Coordinate;
    size: bigint;
    extent: number;
    low?: Coordinate;
    high?: Coordinate;
    slope: number;
    excluded: [Coordinate, Coordinate][];
}
const min = (a: bigint, b: bigint) => a < b ? a : b;
const max = (a: bigint, b: bigint) => a > b ? a : b;
const lower = (a: Axis, v: Coordinate) => { if (!a.low || v.grid > a.low.grid || v.grid === a.low.grid && v.value > a.low.value) a.low = v; };
const upper = (a: Axis, v: Coordinate) => { if (!a.high || v.grid < a.high.grid || v.grid === a.high.grid && v.value < a.high.value) a.high = v; };
const axis = (start: number, size: number): Axis => ({ start: coordinate(start), size: fixed(start + size) - fixed(start), extent: size, slope: 0, excluded: [] });
const beforeEdge = (a: Axis, edge: Coordinate) => coordinate(edge.value - a.extent, edge.grid - a.size);

/** Preserve intersection with the fixed target interval, in target coordinates. */
function keepInterval(a: Axis, lo: Coordinate, hi: Coordinate) {
    if (a.start.grid > lo.grid) { lower(a, a.start); upper(a, a.start); } else upper(a, lo);
    const end = a.start.grid + a.size;
    if (end < hi.grid) { lower(a, a.start); upper(a, a.start); } else lower(a, beforeEdge(a, hi));
}

/** Minimize a linear objective on an interval minus a union of OPEN intervals.
 * The original coordinate is feasible. Zero slope therefore has the unique
 * Euclidean tie-break at the original coordinate. A nonzero slope selects the
 * extreme feasible endpoint. Touching exclusions must not merge: their shared
 * endpoint is feasible. Sorting then sweeping is O(n log n), without a search cap. */
function minimize(a: Axis): Coordinate | undefined {
    const intervals: [Coordinate, Coordinate][] = [];
    for (const interval of a.excluded.sort((a, b) => a[0].grid < b[0].grid ? -1 : a[0].grid > b[0].grid ? 1 : 0)) {
        const last = intervals.at(-1);
        if (last && interval[0].grid < last[1].grid) {
            if (interval[1].grid > last[1].grid) last[1] = interval[1];
        }
        else intervals.push([...interval]);
    }
    let at = a.slope > 0 ? a.low : a.slope < 0 ? a.high : a.start;
    if (at === undefined) return undefined;
    if (a.low !== undefined && at.grid < a.low.grid) at = a.low;
    if (a.high !== undefined && at.grid > a.high.grid) at = a.high;
    if (a.slope < 0) intervals.reverse();
    for (const [lo, hi] of intervals) if (lo.grid < at.grid && at.grid < hi.grid) {
        if (a.slope === 0) {
            const left = a.low === undefined || lo.grid >= a.low.grid, right = a.high === undefined || hi.grid <= a.high.grid;
            if (!left && !right) return undefined;
            at = left && (!right || at.grid - lo.grid <= hi.grid - at.grid) ? lo : hi;
        } else at = a.slope > 0 ? hi : lo;
    }
    return (a.low === undefined || at.grid >= a.low.grid) && (a.high === undefined || at.grid <= a.high.grid) ? at : undefined;
}

/** Outside corner-to-corner contact with the other units' complete bounds.
 * Stable tie order follows B's northwest, northeast, southwest, southeast corners. */
function nearestOutsideCorner(s: Rect, b: Rect): Rect {
    let best = s, distance = Infinity;
    for (const y of [b.y - s.h, b.y + b.h]) for (const x of [b.x - s.w, b.x + b.w]) {
        const d = Math.hypot(x - s.x, y - s.y);
        if (d < distance) { best = { ...s, x, y }; distance = d; }
    }
    return best;
}

/** Per-unit compaction. Every other visible unit participates, regardless of
 * device identity, connectivity or occlusion. See layout.md for the definition.
 * Intersecting unit interiors skip compaction before either case below.
 * With no facing unit, select the nearest outside bounding-corner contact.
 * Otherwise preserve the existing projection-constrained gap objective:
 * Feasibility separates by axis: a facing pair fixes its tangential intersection
 * and its normal half-plane. An empty pair must remain diagonally disjoint (both
 * axis intervals disjoint), since overlap is forbidden. The summed gap is linear
 * on these sets. Thus two sorted sweeps find the global minimum and tie-break. */
function settledRect(s: Rect, others: Rect[]): Rect {
    // Computer bounds can intersect in an L-shaped desktop's empty notch without
    // any actual screens colliding. Compaction must not push those units out.
    // Positive interior overlap on BOTH axes skips only this unit; strict
    // comparisons exclude edge/corner touching. Test the original coordinates
    // before fixed-grid rounding so even sub-grid intersections stay unchanged.
    if (others.some(t => s.x < t.x + t.w && t.x < s.x + s.w
        && s.y < t.y + t.h && t.y < s.y + s.h)) return s;
    const x = axis(s.x, s.w), y = axis(s.y, s.h);
    let hasFacing = false;
    for (const t of others) {
        const tx = coordinate(t.x), ty = coordinate(t.y), right = coordinate(t.x + t.w), bottom = coordinate(t.y + t.h);
        const ox = min(x.start.grid + x.size, right.grid) > max(x.start.grid, tx.grid);
        const oy = min(y.start.grid + y.size, bottom.grid) > max(y.start.grid, ty.grid);
        hasFacing ||= ox || oy;
        if (oy && !ox) {
            keepInterval(y, ty, bottom);
            if (x.start.grid >= right.grid) { lower(x, right); x.slope++; }
            else { upper(x, beforeEdge(x, tx)); x.slope--; }
        } else if (ox && !oy) {
            keepInterval(x, tx, right);
            if (y.start.grid >= bottom.grid) { lower(y, bottom); y.slope++; }
            else { upper(y, beforeEdge(y, ty)); y.slope--; }
        } else {
            x.excluded.push([beforeEdge(x, tx), right]);
            y.excluded.push([beforeEdge(y, ty), bottom]);
        }
    }
    if (!hasFacing && others.length) return nearestOutsideCorner(s, bounds(others));
    const px = minimize(x), py = minimize(y);
    if (px === undefined || py === undefined) return s;
    return { ...s, x: px.value, y: py.value };
}

export function settleScreen(s: Rect, others: Rect[]): Offset {
    const next = settledRect(s, others);
    return next.x === s.x && next.y === s.y ? NONE : { dx: next.x - s.x, dy: next.y - s.y };
}

const compare = (a: string, b: string) => a < b ? -1 : a > b ? 1 : 0;
/** One pass only; every step sees preceding results. Input iteration order is irrelevant. */
export function compactUnits(units: Unit[], edited: (unit: Unit) => boolean = () => true): Unit[] {
    const result = units.map(u => ({ ...u, rect: { ...u.rect } }))
        .sort((a, b) => compare(a.device, b.device));
    result.forEach((u, i) => {
        if (!edited(u)) return;
        const others = result.filter((_, j) => i !== j);
        u.rect = settledRect(u.rect, others.map(u => u.rect));
    });
    return result;
}

/** Every computer contributes one visible bounding rectangle, regardless of
 * system-arrangement sync or connectivity. Hidden screens do not affect bounds. */
export function compactionUnits(screens: Screen[]): Unit[] {
    const devices = [...new Set(screens.map(s => s.device ?? s.id))];
    return devices.map(device => ({ device, rect: bounds(screens.filter(s => (s.device ?? s.id) === device)) }));
}

/** Compact only edited computers, carrying hidden members with their computer.
 * Unedited computers are fixed targets. Omit `edited` for an explicit full pass. */
export function compactScreens(all: Screen[], visible: Screen[], edited?: ReadonlySet<string>): Screen[] {
    const before = compactionUnits(visible);
    const after = compactUnits(before, u => {
        const members = visible.filter(s => (s.device ?? s.id) === u.device);
        return (!edited || members.some(s => edited.has(s.id)));
    });
    return all.map(s => {
        const old = before.find(u => u.device === (s.device ?? s.id));
        const next = old && after.find(u => u.device === old.device);
        if (!old || !next) return s;
        const dx = next.rect.x - old.rect.x, dy = next.rect.y - old.rect.y;
        return dx || dy ? { ...s, x: next.rect.x + (s.x - old.rect.x), y: next.rect.y + (s.y - old.rect.y) } : s;
    });
}
