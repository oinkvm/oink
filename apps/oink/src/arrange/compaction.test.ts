import { describe, expect, it } from 'vitest';
import { compactRectangles, edgeProjections } from './compaction';
import { compactGroups } from './groups';
import type { Screen } from './geometry';

const staggered = [
    { x: 508, y: 0, w: 868, h: 488 },
    { x: 0, y: 525, w: 558, h: 361 },
    { x: 737, y: 543, w: 584, h: 377 },
];

describe('whole-layout contact compaction', () => {
    it('does not compact a visually touching layout with subpixel penetration', () => {
        const rects = [
            { x: 493, y: 0, w: 741, h: 416.8125 },
            { x: 0, y: 416.8125, w: 604, h: 604 * 1117 / 1728 },
            { x: 603.75, y: 472.8125, w: 604, h: 604 * 1117 / 1728 },
        ];
        expect(compactRectangles(rects, ['a', 'b', 'c'])).toEqual(rects);
    });

    it.each([1, 1 / 4096])('retains a lower gap whose closure would slide the upper projection at scale %s', scale => {
        const rects = [
            { x: 0, y: 0, w: 3461.5555555555566, h: 1947.1250000000007 },
            { x: 1139, y: 2185, w: 2055, h: 1328.3767361111109 },
            { x: -1687, y: 1947.125, w: 1966, h: 1270.8460648148146 },
        ].map(r => ({ x: r.x * scale, y: r.y * scale, w: r.w * scale, h: r.h * scale }));
        expect(compactRectangles(rects, ['a', 'b', 'c'])).toEqual(rects);
    });

    it.each([1, 1 / 4096])('closes the nearer staggered gap with identical edge projections at scale %s', scale => {
        const rects = staggered.map(r => ({ x: r.x * scale, y: r.y * scale, w: r.w * scale, h: r.h * scale }));
        const groups = ['a', 'b', 'c'];
        const result = compactRectangles(rects, groups);
        expect(result[0].y).toBeCloseTo(37 * scale, 10);
        expect(result.slice(1)).toEqual(rects.slice(1));
        expect(edgeProjections(result)).toEqual(edgeProjections(rects));
        expect(compactRectangles(result, groups)).toEqual(result);
    });

    it('translates every member uniformly using exposed screen edges', () => {
        const screens: Screen[] = [
            { id: 'a', device: 'one', name: 'First', x: 0, y: 0, w: 100, h: 100, wallpaper: 0 },
            { id: 'b', device: 'one', name: 'First', x: 100, y: 50, w: 100, h: 100, wallpaper: 0 },
            { id: 'c', device: 'two', name: 'Second', x: 100, y: 200, w: 100, h: 100, wallpaper: 0 },
        ];
        const result = compactGroups(screens);
        expect(result[0]).toEqual({ ...screens[0], y: 50 });
        expect(result[1]).toEqual({ ...screens[1], y: 100 });
        expect(result[2]).toEqual(screens[2]);
        expect(edgeProjections(result)).toEqual(edgeProjections(screens));
    });

    it('does not fill the empty space inside a nonrectangular computer bounding box', () => {
        const screens = [
            { x: 0, y: 0, w: 100, h: 100 },
            { x: 200, y: 0, w: 100, h: 100 },
            { x: 120, y: 160, w: 60, h: 100 },
        ];
        expect(compactRectangles(screens, ['one', 'one', 'two'])).toEqual(screens);
    });

    it('keeps existing contact and refuses tangential sliding that changes projected intervals', () => {
        const screens = [
            { x: 0, y: 0, w: 100, h: 100 },
            { x: 100, y: 20, w: 100, h: 60 },
            { x: 300, y: 20, w: 100, h: 60 },
        ];
        const result = compactRectangles(screens, ['a', 'b', 'c']);
        expect(result[0]).toEqual(screens[0]);
        expect(result[1]).toEqual(screens[1]);
        expect(result[2]).toEqual({ ...screens[2], x: 200 });
        expect(edgeProjections(result)).toEqual(edgeProjections(screens));
    });
});
