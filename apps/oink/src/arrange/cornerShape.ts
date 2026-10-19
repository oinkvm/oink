import { useEffect, useState } from 'react';
import type { Corner } from './geometry';
export interface CornerShape {
    n: number;
    rk: number;
}
const DEFAULT: CornerShape = { n: 2, rk: 1 };
export function readCornerShape(): CornerShape {
    if (typeof document === 'undefined')
        return DEFAULT;
    const cs = getComputedStyle(document.documentElement);
    const corner = cs.getPropertyValue('--ok-corner').trim();
    const rk = parseFloat(cs.getPropertyValue('--ok-rk')) || 1;
    const m = /superellipse\(\s*([-\d.]+)\s*\)/.exec(corner);
    const n = m ? 2 ** parseFloat(m[1]) : corner === 'squircle' ? 4 : 2;
    return { n, rk };
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
