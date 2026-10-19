import { describe, expect, it } from 'vitest';
import { projectScreenRect } from './renderGeometry';

describe('screen raster bounds', () => {
    it.each([1, 1.25, 1.5, 2, 3])('keeps shared horizontal and vertical edges flush at %sx', dpr => {
        const camera = { ox: 23.9969, oy: 211.331, zoom: .213333333333 };
        const a = projectScreenRect({ x: -900, y: -600, w: 900, h: 600 }, camera, dpr);
        const right = projectScreenRect({ x: 0, y: -600, w: 750, h: 600 }, camera, dpr);
        const below = projectScreenRect({ x: -900, y: 0, w: 900, h: 550 }, camera, dpr);
        expect(a.x + a.w).toBeCloseTo(right.x, 12);
        expect(a.y + a.h).toBeCloseTo(below.y, 12);
        for (const r of [a, right, below]) {
            for (const edge of [r.x, r.y, r.x + r.w, r.y + r.h])
                expect(edge * dpr).toBeCloseTo(Math.round(edge * dpr), 10);
        }
    });

    it('preserves world geometry and moves each painted edge by at most half a physical pixel', () => {
        const rect = { x: -123.4, y: 17.9, w: 974.4, h: 1670.4 };
        const camera = { ox: 13.33, oy: -4.2, zoom: .1947 };
        const copy = { ...rect };
        const r = projectScreenRect(rect, camera, 2);
        const projected = [camera.ox + rect.x * camera.zoom, camera.oy + rect.y * camera.zoom,
            camera.ox + (rect.x + rect.w) * camera.zoom, camera.oy + (rect.y + rect.h) * camera.zoom];
        [r.x, r.y, r.x + r.w, r.y + r.h].forEach((edge, i) => expect(Math.abs(edge - projected[i])).toBeLessThanOrEqual(.25));
        expect(rect).toEqual(copy);
    });

    it('aligns global edges when the canvas origin is fractional', () => {
        const origin = { x: 316.25, y: 68.375 };
        const camera = { ox: 23.997, oy: 211.331, zoom: .213333333333 };
        const a = projectScreenRect({ x: 0, y: 0, w: 900, h: 600 }, camera, 2, origin);
        const b = projectScreenRect({ x: 900, y: 0, w: 750, h: 600 }, camera, 2, origin);
        expect(a.x + a.w).toBe(b.x);
        for (const edge of [a.x + origin.x, a.y + origin.y, b.x + b.w + origin.x, a.y + a.h + origin.y])
            expect(edge * 2).toBe(Math.round(edge * 2));
    });
});
