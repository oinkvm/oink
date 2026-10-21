import { describe, expect, it } from 'vitest';
import { settleGroup, type Offset } from './compaction';
import { renderLayout } from './adapter';
import { bounds, type Rect } from './geometry';
import { edgeProjections, overlaps, sameProjections, tolerance } from './facing';
import { threeComputers } from './threeComputers.fixture';

const moved = (rects: Rect[], o: Offset) => rects.map(r => ({ ...r, x: r.x + o.dx, y: r.y + o.dy }));
/** Assert the rule's invariants for one settle and return the moved group. */
function settle(moving: Rect[], others: Rect[], groups = others.map((_, i) => String(i))) {
    const offset = settleGroup(moving, others, groups);
    const placed = moved(moving, offset), eps = tolerance([...moving, ...others]);
    expect(sameProjections(edgeProjections([...moving, ...others], eps), edgeProjections([...placed, ...others], eps), eps)).toBe(true);
    for (const m of placed) for (const r of others) expect(overlaps(m, r, eps)).toBe(false);
    return placed;
}

describe('the user layout with hidden Windows screen and a two-screen Mac', () => {
    const all = renderLayout(threeComputers, {});
    // Hidden or disabled screens take no part in compaction.
    const visible = all.filter((_, i) => !threeComputers.geometry.screens[i].hidden);
    const b = visible.filter(s => s.device === 'b'.repeat(64)), c = visible.find(s => s.device === 'c'.repeat(64))!;
    const box = bounds(b);

    it('moves a far bottom-right release diagonally until it touches the other computer\'s bounding rectangle', () => {
        const [placed] = settle([{ ...c, x: 9000, y: 4000 }], b, b.map(s => s.device!));
        expect(placed.x).toBeCloseTo(box.x + box.w, 6);
        expect(placed.y).toBeCloseTo(box.y + box.h, 6);
    });

    it('stops a release to the right at the bounding rectangle when reaching a screen would change a projection', () => {
        // Sliding further left would put the external display above it.
        const [placed] = settle([{ ...c, x: 9000, y: 1500 }], b, b.map(s => s.device!));
        expect(placed.x).toBeCloseTo(box.x + box.w, 6);
        expect(placed.y).toBe(1500);
    });

    it('keeps the stored corner gap because closing it would change the seam with the built-in screen', () => {
        expect(settleGroup([c], b, b.map(s => s.device!))).toEqual({ dx: 0, dy: 0 });
    });

    it('a loose release in the corner keeps its gaps: either closure would change the other seam', () => {
        // Facing the external display above and the built-in screen beside it.
        expect(settleGroup([{ ...c, x: 5600, y: 1200 }], b, b.map(s => s.device!))).toEqual({ dx: 0, dy: 0 });
    });
});

describe('edited-computer compaction', () => {
    it('reaches real screen contact along a facing axis', () => {
        const other = { x: 0, y: 0, w: 1512, h: 982 };
        expect(settle([{ x: 1700, y: 200, w: 1000, h: 600 }], [other])).toEqual([{ x: 1512, y: 200, w: 1000, h: 600 }]);
    });

    it('retains an existing contact instead of moving toward a farther screen', () => {
        const at = { x: 1512, y: 0, w: 800, h: 500 };
        expect(settleGroup([at], [{ x: 0, y: 0, w: 1512, h: 982 }, { x: 4000, y: 0, w: 800, h: 500 }], ['a', 'b'])).toEqual({ dx: 0, dy: 0 });
    });

    it('moves the whole group with its native offsets and picks the nearest computer', () => {
        const group = [{ x: 1000, y: 1000, w: 100, h: 100 }, { x: 1100, y: 1050, w: 100, h: 100 }];
        const placed = settle(group, [{ x: 0, y: 0, w: 100, h: 100 }, { x: 3000, y: 3000, w: 100, h: 100 }], ['near', 'far']);
        expect(placed[1].x - placed[0].x).toBe(100);
        expect(placed[1].y - placed[0].y).toBe(50);
        expect(placed[0]).toMatchObject({ x: 100, y: 100 });
    });

    it.each([1, 1 / 4096])('keeps every projection of the reconstructed rounded-contact layout at scale %s', scale => {
        const rects = [
            { x: 0, y: 0, w: 3461.5555555555566, h: 1947.1250000000007 },
            { x: 1139, y: 2185, w: 2055, h: 1328.3767361111109 },
            { x: -1687, y: 1947.125, w: 1966, h: 1270.8460648148146 },
        ].map(r => ({ x: r.x * scale, y: r.y * scale, w: r.w * scale, h: r.h * scale }));
        for (const [i, r] of rects.entries()) settle([r], rects.filter((_, j) => j !== i));
    });

    it('never changes a projection or overlaps in generated layouts', () => {
        let seed = 0x51c0ffee;
        const random = () => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed / 2 ** 32; };
        for (let i = 0; i < 300; i++) {
            const rect = () => ({ x: Math.floor(random() * 8) * 100, y: Math.floor(random() * 8) * 100, w: 50 + Math.floor(random() * 4) * 50, h: 50 + Math.floor(random() * 4) * 50 });
            const others: Rect[] = [];
            const groups: string[] = [];
            for (let n = 1 + i % 4; others.length < n;) {
                const r = rect();
                if (others.every(o => !overlaps(o, r))) { others.push(r); groups.push(String(others.length % 2)); }
            }
            const moving = [{ ...rect(), x: 1200 + random() * 400, y: 1200 + random() * 400 }];
            const [placed] = settle(moving, others, groups);
            // A computer with nothing in between always ends in contact with something.
            const eps = tolerance([...moving, ...others]);
            const boxes = [...new Set(groups)].map(g => bounds(others.filter((_, j) => groups[j] === g)));
            const gapTo = (r: Rect) => Math.max(r.x - (placed.x + placed.w), placed.x - (r.x + r.w), r.y - (placed.y + placed.h), placed.y - (r.y + r.h));
            expect(Math.min(...[...others, ...boxes].map(gapTo)), `case ${i}`).toBeLessThanOrEqual(eps);
        }
    });
});
