import { describe, expect, it } from 'vitest';
import { compactScreens, compactUnits, settleScreen, type Unit } from './compaction';
import { RENDER_SCALE, renderLayout } from './adapter';
import { cornerClearance, type Rect } from './geometry';
import { diagonalComputers } from './diagonalComputers.fixture';

const box = (x: number, y: number, w = 10, h = 10): Rect => ({ x, y, w, h });
const shifted = (s: Rect, o: { dx: number; dy: number }) => ({ ...s, x: s.x + o.dx, y: s.y + o.dy });
// Independent integer oracle: target-edge projections, before occlusion.
function projections(s: Rect, t: Rect) {
    const x = [Math.max(s.x, t.x), Math.min(s.x + s.w, t.x + t.w)];
    const y = [Math.max(s.y, t.y), Math.min(s.y + s.h, t.y + t.h)];
    return [s.x + s.w <= t.x && y[1] > y[0] ? y : null,
        s.x >= t.x + t.w && y[1] > y[0] ? y : null,
        s.y + s.h <= t.y && x[1] > x[0] ? x : null,
        s.y >= t.y + t.h && x[1] > x[0] ? x : null];
}
const overlap = (a: Rect, b: Rect) => a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
function cost(s: Rect, ts: Rect[]) {
    return ts.reduce((sum, t) => sum + projections(s, t).reduce((n, p, i) => n + (!p ? 0 : [t.x - s.x - s.w, s.x - t.x - t.w, t.y - s.y - s.h, s.y - t.y - t.h][i]), 0), 0);
}
function check(s: Rect, ts: Rect[]) {
    const after = shifted(s, settleScreen(s, ts));
    expect(ts.map(t => projections(after, t))).toEqual(ts.map(t => projections(s, t)));
    expect(ts.some(t => overlap(after, t))).toBe(false);
    return after;
}

describe('projection-constrained per-unit compaction', () => {
    it('preserves every target interval, including occluded targets and empty projections', () => {
        const s = box(20, 5, 20, 20), ts = [box(0, 0, 10, 40), box(-20, 0, 10, 40), box(45, 30)];
        expect(check(s, ts)).toEqual(box(10, 5, 20, 20));
    });
    it('minimizes the sum rather than choosing the nearest contact', () => {
        const s = box(20, 0, 10, 40), ts = [box(0, 0), box(100, 0), box(100, 20)];
        expect(check(s, ts).x).toBe(90);
    });
    it('uses Euclidean displacement to break equal sums, keeping a gap between opposite neighbours', () => {
        const s = box(20, 0), ts = [box(0, 0), box(100, 0)];
        expect(settleScreen(s, ts)).toEqual({ dx: 0, dy: 0 });
    });
    it('does not create overlaps or new projections when approaching a diagonal obstacle', () => {
        const s = box(50, 0, 10, 10), ts = [box(0, 0), box(30, 10)];
        expect(check(s, ts)).toEqual(box(10, 0));
    });
    it('keeps the current position when the two tangential constraints pin it', () => {
        expect(settleScreen(box(20, 20), [box(0, 15), box(15, 0)])).toEqual({ dx: 0, dy: 0 });
    });
    it('moves the large upper-right screen down onto the left lower screen in the reported geometry', () => {
        const s = box(566, 90, 722, 406), ts = [box(127, 804, 492, 317), box(749, 852, 491, 317)];
        expect(check(s, ts)).toEqual({ ...s, y: 398 });
        expect(cost(s, ts)).toBe(664);
        expect(cost(check(s, ts), ts)).toBe(48);
    });
    it('matches exhaustive feasible-position minimization on small integer layouts', () => {
        let seed = 123;
        const random = () => (seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0) % 15;
        for (let i = 0; i < 120; i++) {
            const s = box(random(), random(), 3, 3);
            const ts = Array.from({ length: 4 }, () => box(random(), random(), 3, 3)).filter(t => !overlap(s, t));
            const before = JSON.stringify(ts.map(t => projections(s, t)));
            // The existing objective applies only when there is a facing pair.
            if (!ts.some(t => projections(s, t).some(Boolean))) continue;
            const after = check(s, ts), score = [cost(after, ts), (after.x - s.x) ** 2 + (after.y - s.y) ** 2];
            for (let x = -3; x <= 18; x++) for (let y = -3; y <= 18; y++) {
                const r = { ...s, x, y };
                if (ts.some(t => overlap(r, t)) || JSON.stringify(ts.map(t => projections(r, t))) !== before) continue;
                const c = cost(r, ts), d = (x - s.x) ** 2 + (y - s.y) ** 2;
                expect(c > score[0] || c === score[0] && d >= score[1], `case ${i} at ${x},${y}`).toBe(true);
            }
        }
    });
});

describe('intersecting computer bounds', () => {
    const screen = (id: string, device: string, rect: Rect) => ({ ...rect, id, device, name: id, wallpaper: 0 });
    const lShape = [screen('top', 'a', box(0, 0, 30, 10)), screen('left', 'a', box(0, 10, 10, 20))];
    const notch = screen('notch', 'b', box(15, 15));
    const all = [...lShape, notch];

    it.each(['edit-notch', 'edit-computer', 'global'] as const)('keeps a screen in an L-shaped computer notch unchanged during %s compaction', mode => {
        expect(lShape.some(s => overlap(s, notch))).toBe(false);
        expect(overlap(box(0, 0, 30, 30), notch)).toBe(true);
        const edited = mode === 'global' ? undefined : new Set([mode === 'edit-notch' ? notch.id : lShape[0].id]);
        expect(compactScreens(all, all, edited)).toEqual(all);
    });

    it('skips intersecting units while later units compact in the same fixed pass', () => {
        const input = [...all, screen('far', 'c', box(100, 5, 10, 20)), screen('last', 'd', box(150, 0))];
        const next = compactScreens(input, input);
        expect(next.slice(0, 3)).toEqual(all);
        expect(next[3]).toEqual({ ...input[3], x: 30 });
        expect(next[4]).toEqual({ ...input[4], x: 40 });
        // The last step sees the preceding screen at x=30, not its old x=100.
        const reversed = [...input].reverse();
        expect(compactScreens(reversed, reversed).reverse()).toEqual(next);
    });

    it('skips even a positive interior intersection smaller than the fixed grid', () => {
        const moving = box(30 - 2 ** -22, 10);
        expect(settleScreen(moving, [box(0, 0, 30, 30)])).toEqual({ dx: 0, dy: 0 });
    });

    it('still closes a sub-grid gap instead of skipping it as an intersection', () => {
        const moving = box(30 + 2 ** -22, 10);
        expect(shifted(moving, settleScreen(moving, [box(0, 0, 30, 30)]))).toEqual(box(30, 10));
    });

    it('does not skip a touching edge when the existing facing objective moves away from it', () => {
        const moving = box(30, 10), others = [box(0, 0, 30, 30), box(80, 0, 10, 15), box(80, 15, 10, 15)];
        expect(check(moving, others)).toEqual(box(70, 10));
    });

    it('does not skip corner contact or intersection with only the combined bounds of separate units', () => {
        expect(check(box(30, 30), [box(0, 0, 30, 30), box(80, 80)])).toEqual(box(-10, -10));
    });
});

describe('corner compaction without a facing unit', () => {
    it.each([
        ['northwest', box(-30, -40, 4, 6), box(6, 4, 4, 6)],
        ['northeast', box(40, -40, 4, 6), box(20, 4, 4, 6)],
        ['southwest', box(-30, 40, 4, 6), box(6, 20, 4, 6)],
        ['southeast', box(40, 40, 4, 6), box(20, 20, 4, 6)],
    ] as const)('moves to the nearest outside %s corner', (_, s, expected) => {
        expect(check(s, [box(10, 10)])).toEqual(expected);
    });

    it('uses the bounding rectangle of all other units, with northwest first on an exact distance tie', () => {
        const s = box(23, 23, 4, 4), others = [box(0, 0), box(40, 40)];
        expect(check(s, others)).toEqual(box(-4, -4, 4, 4));
        expect(check(s, [...others].reverse())).toEqual(box(-4, -4, 4, 4));
    });

    it('keeps a sole screen and an existing outside corner contact unchanged', () => {
        expect(settleScreen(box(30, 30), [])).toEqual({ dx: 0, dy: 0 });
        expect(settleScreen(box(10, 10), [box(0, 0)])).toEqual({ dx: 0, dy: 0 });
    });

    it('uses the preceding corner results in fixed device order during the global pass', () => {
        const units = [{ device: 'a', rect: box(0, 0) }, { device: 'b', rect: box(30, 30) }, { device: 'c', rect: box(60, 60) }];
        const result = compactUnits(units);
        expect(result.map(u => u.rect)).toEqual([box(20, 20), box(10, 10), box(30, 30)]);
        expect(compactUnits([...units].reverse())).toEqual(result);
    });

    it('includes offline units, excludes hidden bounds, and carries hidden siblings rigidly after an edit', () => {
        const all = [
            { ...box(40, 40), id: 'a', device: 'a', name: 'a', wallpaper: 0 },
            { ...box(0, 0), id: 'b', device: 'b', name: 'b', wallpaper: 0, offline: true },
            { ...box(200, 200), id: 'hidden-a', device: 'a', name: 'hidden', wallpaper: 0 },
            { ...box(300, 300), id: 'hidden-c', device: 'c', name: 'hidden', wallpaper: 0 },
        ];
        const [moved, target, sibling, hidden] = compactScreens(all, all.slice(0, 2), new Set(['a']));
        expect(moved.x).toBe(10); expect(moved.y).toBe(10);
        expect(sibling.x).toBe(170); expect(sibling.y).toBe(170);
        expect(target).toEqual(all[1]);
        expect(hidden).toEqual(all[3]);
    });

    it.each(['edit', 'global'] as const)('closes the reported large diagonal layout exactly during %s compaction', mode => {
        const all = renderLayout(diagonalComputers, {});
        const edited = mode === 'edit' ? new Set([all[0].id]) : undefined;
        const [right, left] = compactScreens(all, all, edited);
        expect(right.x).toBe(left.x + left.w);
        expect(right.y).toBe(left.y + left.h);
        expect(cornerClearance(left, [right]).se).toBe(0);
        expect(cornerClearance(right, [left]).nw).toBe(0);
        expect(mode === 'edit' ? left : right).toEqual(mode === 'edit' ? all[1] : all[0]);
    });
});

describe('fixed single-pass order', () => {
    const units: Unit[] = [{ device: 'c', rect: box(100, 0) }, { device: 'a', rect: box(0, 0) }, { device: 'b', rect: box(30, 0) }];
    it('visits every unit exactly once in device order, without a local anchor', () => {
        const result = compactUnits(units);
        expect(result.map(u => [u.device, u.rect.x])).toEqual([['a', 20], ['b', 30], ['c', 40]]);
        expect(compactUnits([...units].reverse())).toEqual(result);
    });
    it('compacts only edited units while considering all the others, including offline units', () => {
        expect(compactUnits(units, u => u.device === 'c').map(u => u.rect.x)).toEqual([0, 30, 40]);
    });
});

describe('fractional contact geometry', () => {
    // Reference-plane dimensions and large origins from the reported layout;
    // a partial tangential overlap reproduces the two contacting corner shapes.
    const w = .5018144325252422 * RENDER_SCALE, h = .3243788895432266 * RENDER_SCALE;
    const target = box(45.04925694126924 * RENDER_SCALE, 24.03391521322931 * RENDER_SCALE, w, h);
    it.each(['left', 'right', 'top', 'bottom'] as const)('closes the %s gap at the original edge, without fixed-point residue', side => {
        const horizontal = side === 'left' || side === 'right';
        const moving = box(
            horizontal ? (side === 'left' ? .5163916920004609 : 46.04925694126924) * RENDER_SCALE : target.x + w / 10,
            horizontal ? target.y + h / 10 : (side === 'top' ? .5163916920004609 : 25.03391521322931) * RENDER_SCALE,
            w, h,
        );
        const after = shifted(moving, settleScreen(moving, [target]));
        const gap = side === 'left' ? target.x - (after.x + after.w)
            : side === 'right' ? after.x - (target.x + target.w)
            : side === 'top' ? target.y - (after.y + after.h) : after.y - (target.y + target.h);
        expect(gap).toBe(0);
        expect(horizontal ? after.y : after.x).toBe(horizontal ? moving.y : moving.x);
    });

    it('squares only the contacting corners after the complete screen compaction pass', () => {
        const screen = (id: string, r: Rect) => ({ ...r, id, device: id, name: id, wallpaper: 0 });
        const all = [screen('a', target), screen('b', box(46.04925694126924 * RENDER_SCALE, target.y + h / 10, w, h))];
        const [left, right] = compactScreens(all, all);
        expect(right.x).toBe(left.x + left.w);
        const leftCorners = cornerClearance(left, [right]), rightCorners = cornerClearance(right, [left]);
        expect(leftCorners.se).toBe(0);
        expect(rightCorners.nw).toBe(0);
        expect(leftCorners.ne).toBeGreaterThan(0);
        expect(rightCorners.sw).toBeGreaterThan(0);
        expect(leftCorners.nw).toBeNull();
        expect(leftCorners.sw).toBeNull();
        expect(rightCorners.ne).toBeNull();
        expect(rightCorners.se).toBeNull();
        expect(compactScreens([left, right], [left, right])).toEqual([left, right]);
    });

    it('closes a sub-grid gap instead of treating the rounded origin as the destination', () => {
        const moving = box(target.x + w + 2 ** -20, target.y + h / 10, w, h);
        const [after] = compactUnits([{ device: 'a', rect: moving }, { device: 'b', rect: target }], u => u.device === 'a');
        expect(after.rect.x).toBe(target.x + target.w);
        expect(after.rect.y).toBe(moving.y);
    });

    it('retains the original edge when an open exclusion determines the stopping point', () => {
        const obstacle = box(target.x + w / 2, target.y + h, w, h);
        const moving = box(46.04925694126924 * RENDER_SCALE, target.y, w, h);
        const [after] = compactUnits([{ device: 'a', rect: moving }, { device: 'b', rect: target }, { device: 'c', rect: obstacle }], u => u.device === 'a');
        expect(after.rect.x).toBe(obstacle.x + obstacle.w);
        expect(after.rect.y).toBe(moving.y);
        expect(cornerClearance(after.rect, [obstacle]).sw).toBe(0);
    });
});


it('uses each rigid computer visible bounding rectangle and carries hidden members with its translation', async () => {
    const { compactScreens } = await import('./compaction');
    const screen = (id: string, device: string, r: Rect) => ({ ...r, id, device, name: id, wallpaper: 0 });
    const all = [screen('a1', 'a', box(0, 0)), screen('a2', 'a', box(10, 10)),
        screen('hidden', 'a', box(200, 200)), screen('b1', 'b', box(50, 0, 10, 20))];
    const visible = all.filter(s => s.id !== 'hidden');
    const next = compactScreens(all, visible, new Set(['a1', 'a2']));
    expect(next.map(s => s.x)).toEqual([30, 40, 230, 50]);
    expect(all.map(s => s.x)).toEqual([0, 10, 200, 50]);
});

it('compacts an offline sync-on computer as a rigid unit with its stored internal geometry', async () => {
    const { compactScreens, compactionUnits } = await import('./compaction');
    const screen = (id: string, device: string, r: Rect, offline = false) => ({ ...r, id, device, name: id, wallpaper: 0, offline });
    const all = [screen('a1', 'a', box(0, 0), true), screen('a2', 'a', box(10, 10), true), screen('b1', 'b', box(50, 0, 10, 20))];
    expect(compactionUnits(all).filter(u => u.device === 'a')).toHaveLength(1);
    const next = compactScreens(all, all, new Set(['a1']));
    expect(next.map(s => s.x)).toEqual([30, 40, 50]);
    expect(next[1].y - next[0].y).toBe(10);
});

it('uses bounding rectangles for both the edited computer and multi-screen targets', async () => {
    const { compactScreens, compactionUnits } = await import('./compaction');
    const screen = (id: string, device: string, r: Rect) => ({ ...r, id, device, name: id, wallpaper: 0 });
    const all = [screen('a1', 'a', box(0, 0)), screen('a2', 'a', box(10, 5)),
        screen('b1', 'b', box(50, 0)), screen('b2', 'b', box(60, 10))];
    const units = compactionUnits(all);
    expect(units).toEqual([{ device: 'a', rect: box(0, 0, 20, 15) }, { device: 'b', rect: box(50, 0, 20, 20) }]);
    const next = compactScreens(all, all, new Set(['a2']));
    expect(next.map(s => s.x)).toEqual([30, 40, 50, 60]);
    expect(next.map(s => s.y)).toEqual(all.map(s => s.y));
    expect(projections(compactionUnits(next)[0].rect, units[1].rect)).toEqual(projections(units[0].rect, units[1].rect));
    // Selecting both siblings visits the same computer only once.
    expect(compactScreens(all, all, new Set(['a1', 'a2']))).toEqual(next);
});

it('compacts only the edited computer and leaves other screens exactly in place', () => {
    const screen = (id: string, x: number) => ({ ...box(x, 0), id, device: id, name: id, wallpaper: 0 });
    const all = [screen('a', 0), screen('b', 30), screen('c', 100)];
    const next = compactScreens(all, all, new Set(['c']));
    expect(next.map(s => s.x)).toEqual([0, 30, 40]);
    expect(next.slice(0, 2)).toEqual(all.slice(0, 2));
    expect(compactScreens([...all].reverse(), [...all].reverse(), new Set(['c'])).reverse()).toEqual(next);
    expect(compactScreens(all, all, new Set())).toEqual(all);
});

it('visits only selected computers in identity order, independent of input or selection order', () => {
    const screen = (id: string, x: number) => ({ ...box(x, 0), id, device: id, name: id, wallpaper: 0 });
    const all = [screen('a', 0), screen('b', 30), screen('c', 100), screen('d', 200)];
    const next = compactScreens(all, all, new Set(['a', 'c']));
    expect(next.map(s => s.x)).toEqual([20, 30, 40, 200]);
    expect(next[1]).toEqual(all[1]);
    expect(next[3]).toEqual(all[3]);
    expect(compactScreens([...all].reverse(), [...all].reverse(), new Set(['c', 'a'])).reverse()).toEqual(next);
});
