import type { Rect } from './geometry';
import { edgeProjections, sameProjections, tolerance, extent, facing, overlaps } from './facing';
export { edgeProjections } from './facing';

/** Translate whole computers to first contact, preserving every directed edge
 * interval and destination throughout the move. Existing contacts never reopen.
 * All groups participate, including computers other than the last edited one. */
export function compactRectangles<T extends Rect>(
    rects: T[],
    groups: string[],
    movable?: ReadonlySet<string>,
): T[] {
    const eps = tolerance(rects);
    if (rects.some((a, i) => rects.slice(i + 1).some(b => overlaps(a, b, eps)))) return rects;
    let current = rects;
    const devices = [...new Set(groups)].sort();
    const before = edgeProjections(current, eps);
    while (true) {
        let best: { distance: number; next: T[] } | undefined;
        for (const device of devices) {
            if (movable && !movable.has(device)) continue;
            const moving = groups.map(g => g === device);
            for (const axis of ['x', 'y'] as const) {
                const gaps = [-1, 1].map(dir => {
                    let gap = Infinity;
                    current.forEach((a, i) => {
                        if (!moving[i]) return;
                        current.forEach((b, j) => {
                            if (moving[j]) return;
                            const side = (axis === 'y' ? 2 : 0) + (dir > 0 ? 1 : 0);
                            const f = facing(a, b, side, eps);
                            if (f) gap = Math.min(gap, f.gap);
                        });
                    });
                    return gap;
                });
                if (gaps.some(g => g <= eps)) continue;
                for (const [index, distance] of gaps.entries()) {
                    if (!Number.isFinite(distance) || (best && distance >= best.distance - eps)) continue;
                    const dir = index ? 1 : -1;
                    const translated = (t: number) => current.map((r, i) =>
                        moving[i] ? { ...r, [axis]: r[axis] + dir * t } : r);
                    // Projection ownership can change only at endpoint coincidences.
                    // Check each open interval as well as contact, so passing across a
                    // temporary neighbour cannot be hidden by identical final edges.
                    const events = [0, distance];
                    current.forEach((a, i) => {
                        if (!moving[i]) return;
                        current.forEach((b, j) => {
                            if (moving[j]) return;
                            for (const p of [a[axis], a[axis] + extent(a, axis)])
                                for (const q of [b[axis], b[axis] + extent(b, axis)]) {
                                    const t = dir * (q - p);
                                    if (t > eps && t < distance - eps) events.push(t);
                                }
                        });
                    });
                    events.sort((a, b) => a - b);
                    const samples = [distance, ...events.slice(1).map((t, i) => (events[i] + t) / 2)];
                    if (samples.every(t => sameProjections(before, edgeProjections(translated(t), eps), eps)))
                        best = { distance, next: translated(distance) };
                }
            }
        }
        if (!best) return current;
        current = best.next;
        // Each accepted move adds a contact. Projection preservation prevents
        // tangential sliding, and the zero-gap guard prevents opening contacts.
    }
}
