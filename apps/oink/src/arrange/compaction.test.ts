import { describe, expect, it } from 'vitest';
import { compactAround, settleGroup, type Computer, type Offset } from './compaction';
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

describe('one compaction pass when auto-compact is turned on', () => {
    const box = (x: number, y: number, w = 100, h = 100): Rect => ({ x, y, w, h });
    const shifted = (c: Computer, offsets: Map<string, Offset>) => moved(c.rects, offsets.get(c.device) ?? { dx: 0, dy: 0 });
    /** The whole pass keeps every projection and never overlaps; the anchor never moves;
     * passes are bounded by the computer count, and a second run moves nothing. */
    function check(computers: Computer[], anchor: string | null) {
        const result = compactAround(computers, anchor);
        const before = computers.flatMap(c => c.rects), after = computers.flatMap(c => shifted(c, result.offsets)), eps = tolerance(before);
        expect(sameProjections(edgeProjections(before, eps), edgeProjections(after, eps), eps)).toBe(true);
        for (const [i, a] of after.entries()) for (const b of after.slice(i + 1)) expect(overlaps(a, b, eps)).toBe(false);
        if (anchor) expect(result.offsets.has(anchor)).toBe(false);
        expect(result.moves.length).toBeLessThanOrEqual(computers.length);
        const settled = computers.map(c => ({ device: c.device, rects: shifted(c, result.offsets) }));
        expect(compactAround(settled, anchor).offsets.size).toBe(0);
        return result;
    }

    it('keeps this computer fixed and moves the others toward it, nearest first', () => {
        const computers = [{ device: 'a', rects: [box(0, 0)] }, { device: 'c', rects: [box(300, 0)] }, { device: 'b', rects: [box(150, 0)] }];
        const { offsets, moves } = check(computers, 'a');
        expect(offsets.get('b')).toEqual({ dx: -50, dy: 0 });
        expect(offsets.get('c')).toEqual({ dx: -100, dy: 0 });
        expect(moves).toEqual([['b', 'c']]);
    });

    it('keeps the anchor fixed even when it is the computer apart from the others', () => {
        const computers = [{ device: 'z', rects: [box(1000, 0)] }, { device: 'a', rects: [box(0, 0)] }, { device: 'b', rects: [box(300, 0)] }];
        const { offsets, moves } = check(computers, 'z');
        // b goes first (nearer to z) but, as in any compaction move, to its nearest computer;
        // a then keeps its new contact with b. z itself never moves.
        expect(offsets.get('b')).toEqual({ dx: -200, dy: 0 });
        expect(offsets.has('a')).toBe(false);
        expect(moves).toEqual([['b']]);
    });

    it('breaks distance ties by device ID', () => {
        const computers = [{ device: 'anchor', rects: [box(0, 200)] }, { device: 'c', rects: [box(0, 400)] }, { device: 'b', rects: [box(0, 0)] }];
        expect(check(computers, 'anchor').moves).toEqual([['b', 'c']]);
    });

    it('converges within the bound on a scattered staircase', () => {
        const computers = [0, 1, 2, 3, 4, 5].map(i => ({ device: `d${i}`, rects: [box(i * 170, i * 130, 100 + i * 10, 80)] }));
        expect(check(computers, 'd0').offsets.size).toBeGreaterThan(0);
    });

    it('moves nothing in an already compact layout', () => {
        const computers = [{ device: 'a', rects: [box(0, 0)] }, { device: 'b', rects: [box(100, 0)] }, { device: 'c', rects: [box(0, 100)] }];
        expect(check(computers, 'a')).toEqual({ offsets: new Map(), moves: [] });
    });

    it('works without a visible anchor, by device order alone', () => {
        const computers = [{ device: 'b', rects: [box(150, 0)] }, { device: 'a', rects: [box(0, 0)] }];
        expect(check(computers, null).offsets.size).toBe(1);
    });

    it('keeps every projection of the user layout, without its hidden Windows screen', () => {
        const all = renderLayout(threeComputers, {});
        const visible = all.filter((_, i) => !threeComputers.geometry.screens[i].hidden);
        const computers = [...new Set(visible.map(s => s.device!))].map(device => ({ device, rects: visible.filter(s => s.device === device) }));
        expect(computers.map(c => c.device)).not.toContain('a'.repeat(64));
        // This Mac is the anchor; the other Mac keeps its stored corner gap (see above).
        expect(check(computers, 'c'.repeat(64)).offsets.size).toBe(0);
    });

    it('never changes a projection, overlaps or moves the anchor in generated layouts', () => {
        let seed = 0x7a11ed;
        const random = () => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed / 2 ** 32; };
        for (let i = 0; i < 120; i++) {
            const computers: Computer[] = [];
            for (let n = 2 + i % 4, tries = 0; computers.length < n && tries < 200; tries++) {
                const first = box(Math.floor(random() * 12) * 100, Math.floor(random() * 12) * 100, 50 + Math.floor(random() * 4) * 50, 50 + Math.floor(random() * 4) * 50);
                const rects = random() < .4 ? [first, { ...first, x: first.x + first.w, h: first.h / 2 }] : [first];
                if (computers.every(c => c.rects.every(o => rects.every(r => !overlaps(o, r))))) computers.push({ device: `d${computers.length}`, rects });
            }
            check(computers, 'd0');
        }
    });
});
