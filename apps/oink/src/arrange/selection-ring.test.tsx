// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { render } from '@testing-library/react';
import { beforeAll, describe, expect, it, vi } from 'vitest';
import { Arrangement } from './Arrangement';
import { ringBandPath } from './cornerShape';
import type { Screen } from './geometry';

/**
 * On WebKit (no corner-shape), oink-ui's corner fallback repaints the box-shadows and
 * outlines of any element whose --ok-shape is a squircle as a border-image patch. The
 * patch trails a shadow transition by a frame (a fresh selection flashed square) and
 * at square corners paints the outer band 1 px beyond the native one (a bump). Canvas
 * selection rings are therefore SVG (SelectionRing), never shadows on such elements.
 */
describe('canvas selection rings', () => {
  it('are not spread shadows, outlines or shadow transitions on squircle canvas elements', () => {
    const css = readFileSync(join(process.cwd(), 'src/app.css'), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
    const rules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)].filter(([, selector]) => /\.scr(\b|_|-)/.test(selector));
    expect(rules.length).toBeGreaterThan(0);
    for (const [, selector, body] of rules) {
      const squircle = /(^|;)\s*(--ok-shape|corner-shape):\s*var\(--ok-corner\)/.test(body);
      if (squircle) {
        expect(body, selector.trim()).not.toMatch(/(^|;)\s*box-shadow:\s*(?!none)/);
        expect(body, selector.trim()).not.toMatch(/(^|;)\s*outline:\s*(?!none)/);
      }
      expect(body, selector.trim()).not.toMatch(/transition:[^;]*box-shadow/);
    }
  });

  it('keep square corners square and reach exactly their outer offset', () => {
    const numbers = (d: string) => (d.match(/-?\d+(\.\d+)?/g) ?? []).map(Number);
    const accent = ringBandPath(100, 60, [0, 12, 12, 0], [1.75, 1.75, 1.75, 1.75], 2.5, 4.5);
    const all = numbers(accent);
    expect(Math.min(...all)).toBeCloseTo(-4.5, 9);
    expect(Math.max(...all)).toBeCloseTo(104.5, 9);
    // Both square corners of the outer outline are exact points.
    expect(accent).toMatch(/-4\.5[ ,]+-4\.5/);
    expect(accent).toMatch(/-4\.5[ ,]+64\.5/);
  });

  describe('rendering', () => {
    beforeAll(() => {
      vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
      vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(() => ({ left: 0, top: 0, x: 0, y: 0, width: 900, height: 600, right: 900, bottom: 600, toJSON() {} }));
      Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
    });
    const screen = (id: string, x: number, device = id): Screen => ({ id, device, name: id, x, y: 0, w: 1920, h: 1080, wallpaper: 0, groupSize: 1 });
    const settings = { showHotZones: false, snapping: true, compact: false, padding: 24, step: 10 };

    it('draws one SVG ring on the first render of a selection, for a screen and for a computer', () => {
      const one = render(<Arrangement screens={[screen('a', 0), screen('b', 2000)]} selectedId="a" onSelect={() => {}} onScreensChange={() => {}} settings={settings}/>);
      expect(one.container.querySelectorAll('.scr__ring')).toHaveLength(1);
      expect(one.container.querySelector('.scr[data-selected] .scr__ring')!.querySelectorAll('path')).toHaveLength(2);
      one.unmount();
      const pair = [{ ...screen('m', 0, 'pc'), groupSize: 2 }, { ...screen('s', 1920, 'pc'), groupSize: 2 }];
      const group = render(<Arrangement screens={pair} selectedId="m" onSelect={() => {}} onScreensChange={() => {}} settings={settings}/>);
      expect(group.container.querySelectorAll('.scr__group-border .scr__ring')).toHaveLength(1);
      expect(group.container.querySelectorAll('.scr__ring')).toHaveLength(1);
    });
  });
});
