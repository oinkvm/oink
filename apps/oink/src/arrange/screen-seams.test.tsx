// @vitest-environment jsdom
import { cleanup, render } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { Arrangement } from './Arrangement';
import type { Screen } from './geometry';

beforeEach(() => {
    vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });
    vi.stubGlobal('requestAnimationFrame', () => 0);
    vi.stubGlobal('cancelAnimationFrame', () => {});
    vi.stubGlobal('matchMedia', () => ({ matches: false, addEventListener() {}, removeEventListener() {} }));
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(() => ({
        x: .25, y: .375, left: .25, top: .375, width: 900, height: 600, right: 900.25, bottom: 600.375, toJSON() {},
    }));
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it.each([1, 1.25, 1.5, 2, 3].flatMap(dpr => ['horizontal', 'vertical'].flatMap(axis =>
    (['mac', 'win'] as const).map(platform => ({ dpr, axis, platform })))))(
    'preserves exact $axis contacts on $platform at $dpr× and a fractional canvas origin', ({ dpr, axis, platform }) => {
    vi.stubGlobal('devicePixelRatio', dpr);
    // Reported build-3028 geometry (revision 16), stripped of device identities.
    const screens: Screen[] = [
        { id: 'top', name: 'Top', wallpaper: 0, x: .6042546317341468 * 4096, y: -.47313286504995783 * 4096,
            w: .8411250934221473 * 4096, h: .47313286504995783 * 4096 },
        { id: 'bottom', name: 'Bottom', wallpaper: 0, x: .8098609262515524 * 4096, y: 0,
            w: .5018144325252422 * 4096, h: .3243788895432266 * 4096 },
    ];
    if (axis === 'vertical') screens.forEach(s => {
        [s.x, s.y, s.w, s.h] = [s.y, s.x, s.h, s.w];
    });
    const { container } = render(<Arrangement platform={platform} screens={screens} selectedId={null} onSelect={() => {}} onScreensChange={() => {}}
        settings={{ showHotZones: false, snapping: true, compact: true, padding: 48, step: 10 }}/>);
    const [top, bottom] = [...container.querySelectorAll<HTMLElement>('.scr')].map(el => el.style);
    // Per-tile translates make WebKit rasterize the surfaces in different local
    // pixel phases even when getBoundingClientRect reports the same global edge.
    for (const style of [top, bottom]) {
        if (platform === 'mac') expect(style.transform).toBe('');
        else {
            // Chromium needs translates to retain fractional-DPR coordinates
            // below CSS layout's 1/64 px precision; left/top would add a seam.
            expect(style.transform).toMatch(/^translate\(/);
            expect(style.left).toBe('');
            expect(style.top).toBe('');
        }
    }
    const position = (style: CSSStyleDeclaration) => platform === 'mac'
        ? [parseFloat(style.left), parseFloat(style.top)] : style.transform.match(/-?[\d.]+/g)!.map(Number);
    const horizontal = axis === 'horizontal';
    const index = horizontal ? 1 : 0;
    const edge = position(top)[index] + parseFloat(horizontal ? top.height : top.width);
    const origin = horizontal ? .375 : .25;
    expect(edge).toBeCloseTo(position(bottom)[index], 12);
    expect((edge + origin) * dpr).toBeCloseTo(Math.round((edge + origin) * dpr), 10);
});
