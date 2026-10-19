import { describe, it, expect } from 'vitest';
import { bounds, type Screen } from './geometry';
import {
    freeGroup,
    growGroupAvoiding,
    groupScreen,
    mergeVisibleGroups,
    snapGroup,
    snapGroupResize,
    transformGroup,
} from './groups';

const screens: Screen[] = [
    { id: 'a', device: 'one', name: 'One', wallpaper: 0, x: 0, y: 0, w: 1000, h: 600 },
    { id: 'b', device: 'one', name: 'One', wallpaper: 1, x: 1000, y: 200, w: 800, h: 500 },
];

describe('computer geometry', () => {
    it('uses one bounding rectangle and preserves native proportions', () => {
        expect(groupScreen(screens, 'b')).toMatchObject({ x: 0, y: 0, w: 1800, h: 700 });
        const scaled = transformGroup(screens, bounds(screens), { x: 300, y: 400, w: 3600, h: 1400 });
        expect(scaled[1]).toMatchObject({ x: 2300, y: 800, w: 1600, h: 1000 });
    });
    it('snaps a step in the outer desktop shape against another computer', () => {
        const from = bounds(screens), edges = {};
        const result = snapGroup(
            screens, from, { ...from, x: 3, y: 0 },
            [{ x: 1000, y: -600, w: 600, h: 600 }], 10, edges,
        );
        expect(result.x).toBe(0);
        expect(edges).toHaveProperty('x');
    });
    it('resolves collision with one translation and allows empty native desktop gaps', () => {
        const members = [
            { x: 0, y: 0, w: 100, h: 100 },
            { x: 200, y: 0, w: 100, h: 100 },
        ];
        const from = bounds(members);
        expect(freeGroup(members, from, from, [{ x: 120, y: 0, w: 60, h: 100 }])).toEqual(from);
        const free = freeGroup(members, from, { ...from, x: 30 }, [{ x: 120, y: 0, w: 60, h: 100 }]);
        expect(free.x).toBe(20);
        expect(transformGroup(members, from, free)[1].x).toBe(220);
    });
    it('carries visible edits through hidden screens before undo history and saving', () => {
        const next = mergeVisibleGroups(screens, [{ ...screens[1], x: 2000, y: 500, w: 1600, h: 1000 }]);
        expect(next[0]).toMatchObject({ x: 0, y: 100, w: 2000, h: 1200 });
        expect(next[1]).toMatchObject({ x: 2000, y: 500, w: 1600, h: 1000 });
    });
    it('snaps a group resize without moving the fixed corner', () => {
        const from = bounds(screens);
        const edges = {};
        const to = { ...from, w: 1980, h: 770 };
        const result = snapGroupResize(
            screens, from, to, 'se', [{ x: 1990, y: 200, w: 100, h: 600 }], 20, 640, edges,
        );
        expect(result.x).toBe(0);
        expect(result.y).toBe(0);
        expect(result.w).toBeCloseTo(1990);
        expect(result.h / result.w).toBeCloseTo(from.h / from.w);
    });
});

it('never snaps an edge covered by another screen of the same computer', () => {
    const members = [
        { x: 0, y: 0, w: 1000, h: 600 },
        { x: 1000, y: 0, w: 1000, h: 600 },
    ];
    const from = bounds(members);
    const edges = {};
    const result = snapGroup(members, from, from, [{ x: 1000, y: 100, w: 100, h: 100 }], 10, edges);
    expect(result.x).toBe(0);
    expect(edges).not.toHaveProperty('x');
});

it('keyboard scaling allows another computer to remain in a native desktop gap', () => {
    const group = [
        { x: 0, y: 0, w: 100, h: 100 },
        { x: 200, y: 0, w: 100, h: 100 },
    ];
    const target = growGroupAvoiding(group, bounds(group), 315, [{ x: 120, y: 0, w: 60, h: 100 }], 0);
    expect(target).not.toBeNull();
    expect(target!.w).toBe(315);
});
