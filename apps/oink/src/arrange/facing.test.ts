import { describe, expect, it } from 'vitest';
import { edgeProjections, facing } from './facing';
import { freeGroup } from './groups';
import type { Rect } from './geometry';

const screenshot = [
    { x: 493, y: 0, w: 741, h: 416.8125 },
    { x: 0, y: 416.8125, w: 604, h: 604 * 1117 / 1728 },
    { x: 604, y: 472.8125, w: 604, h: 604 * 1117 / 1728 },
];
const scaled = (rs: Rect[], k: number) => rs.map(r => ({ x: r.x * k, y: r.y * k, w: r.w * k, h: r.h * k }));

describe('shared facing definition', () => {
    it('finds all six directed screenshot projections', () => {
        for (const k of [1, 1 / 4096]) {
            const rs = scaled(screenshot, k), p = edgeProjections(rs);
            expect(p.filter(p => p.kind === 'pair')).toHaveLength(6);
            expect(p.filter(p => p.kind === 'visible' && p.target !== null)).toHaveLength(6);
        }
    });
    it('resolves subpixel penetration without moving a visually touching screen', () => {
        const origin = screenshot[2], moved = { ...origin, x: origin.x - .25 };
        expect(freeGroup([origin], origin, moved, screenshot.slice(0, 2))).toEqual(origin);
    });
    it('includes roundoff contact and short positive overlap but excludes corners and penetration', () => {
        const a = { x: 0, y: 0, w: 100, h: 100 };
        expect(facing(a, { ...a, x: 100 - 1e-9, y: 99.99 }, 1)).not.toBeNull();
        expect(facing(a, { ...a, x: 100, y: 100 }, 1)).toBeNull();
        expect(facing(a, { ...a, x: 99.75 }, 1)).toBeNull();
    });
});
