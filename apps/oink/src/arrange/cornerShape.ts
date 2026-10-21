import { useEffect, useState } from 'react';
import { clampCornerRadii, cornerShapePath, parseCornerShape } from 'oink-ui';
import type { CornerKs, CornerRadii } from 'oink-ui';
import type { Corner } from './geometry';
export interface CornerShape {
    n: number;
    rk: number;
    /** Per-corner curvature of `--ok-corner` (top-left, top-right, bottom-right, bottom-left). */
    ks: CornerKs;
}
const ROUND: CornerKs = [1, 1, 1, 1];
const DEFAULT: CornerShape = { n: 2, rk: 1, ks: ROUND };
export function readCornerShape(): CornerShape {
    if (typeof document === 'undefined')
        return DEFAULT;
    const cs = getComputedStyle(document.documentElement);
    const corner = cs.getPropertyValue('--ok-corner').trim();
    const rk = parseFloat(cs.getPropertyValue('--ok-rk')) || 1;
    const m = /superellipse\(\s*([-\d.]+)\s*\)/.exec(corner);
    const n = m ? 2 ** parseFloat(m[1]) : corner === 'squircle' ? 4 : 2;
    return { n, rk, ks: (corner && parseCornerShape(corner)) || ROUND };
}
export function useCornerShape(): CornerShape {
    const [shape, setShape] = useState(DEFAULT);
    useEffect(() => setShape(readCornerShape()), []);
    return shape;
}
export function cornerArcPath(c: Corner, W: number, H: number, gap: number, r: number, n: number, steps = 24): string {
    const sx = c[1] === 'e' ? 1 : -1;
    const sy = c[0] === 's' ? 1 : -1;
    const R = Math.max(0, Math.min(r, (W + 2 * gap) / 2, (H + 2 * gap) / 2));
    const cx = sx > 0 ? W + gap - R : -gap + R;
    const cy = sy > 0 ? H + gap - R : -gap + R;
    const e = 2 / n;
    let d = '';
    for (let i = 0; i <= steps; i++) {
        const t = (i / steps) * (Math.PI / 2);
        const x = cx + sx * R * Math.cos(t) ** e;
        const y = cy + sy * R * Math.sin(t) ** e;
        d += (i ? 'L' : 'M') + x.toFixed(2) + ' ' + y.toFixed(2);
    }
    return d;
}

/**
 * The selection ring's two bands (canvas gap, then accent) as nonzero ring paths in
 * the box's own coordinates: the box outline offset by `from` and, reversed, by `to`.
 * Rounded corners grow with the offset and square ones stay square, as CSS spread
 * shadows do; the curve is oink-ui's, the one Chromium uses for corner-shape.
 *
 * Why SVG and not a spread box-shadow: on WebKit (no corner-shape) oink-ui's corner
 * fallback repaints a squircle element's shadows as a border-image patch. The patch
 * follows a box-shadow transition a frame late, so a fresh selection first shows the
 * native square ring, and at square (contact) corners it paints the outer band 1 px
 * beyond the native one, a visible bump. A ring drawn here is one element, correct on
 * the first frame, and the same on WebKit and Chromium.
 */
export function ringBandPath(w: number, h: number, radii: readonly [number, number, number, number], ks: CornerKs, from: number, to: number): string {
    const outline = (d: number, reverse: boolean) => {
        const box = { x: -d, y: -d, w: w + 2 * d, h: h + 2 * d };
        const r = radii.map(v => v > 0 ? [v + d, v + d] : [0, 0]) as CornerRadii;
        return cornerShapePath(box, clampCornerRadii(box.w, box.h, r), ks, reverse);
    };
    return `${outline(to, false)} ${outline(from, true)}`;
}
