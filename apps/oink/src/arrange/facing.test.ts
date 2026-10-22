import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { edgeProjections, facing, sameProjections, tolerance } from './facing';
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
    it('matches Rust for generated pairs and visible intervals', () => {
        let seed = 0x51c0ffee;
        const random = () => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed / 2 ** 32; };
        const cases: { rects: Rect[] }[] = [];
        for (const k of [1, 1 / 4096, 16]) {
            for (const penetration of [0, 1e-8, .25]) {
                const rs = screenshot.map(r => ({ ...r }));
                rs[1].y -= penetration;
                rs[2].x -= penetration;
                cases.push({ rects: scaled(rs, k) });
            }
            for (let i = 0; i < 64; i++) {
                const rs = Array.from({ length: 2 + i % 5 }, () => ({
                    x: Math.floor(random() * 5) * 150,
                    y: Math.floor(random() * 5) * 150,
                    w: 50 + Math.floor(random() * 3) * 50,
                    h: 50 + Math.floor(random() * 3) * 50,
                }));
                cases.push({ rects: scaled(rs, k) });
            }
        }
        const output = execFileSync('cargo', ['run', '-j2', '--quiet', '-p', 'oink-display', '--example', 'layout_conformance'], {
            cwd: fileURLToPath(new URL('../../../../', import.meta.url)),
            env: { ...process.env, CARGO_INCREMENTAL: '0', CARGO_PROFILE_DEV_DEBUG: '0', CARGO_PROFILE_TEST_DEBUG: '0' },
            input: JSON.stringify(cases), encoding: 'utf8', maxBuffer: 16 * 1024 * 1024, timeout: 120000,
        });
        const results = JSON.parse(output) as { projections: ReturnType<typeof edgeProjections> }[];
        cases.forEach(({ rects }, i) => {
            expect(sameProjections(edgeProjections(rects), results[i].projections, tolerance(rects)), `pairs case ${i}`).toBe(true);
        });
    }, 125000);
});
