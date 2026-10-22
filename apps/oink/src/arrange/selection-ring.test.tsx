// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { cleanup, render } from '@testing-library/react';
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { Arrangement } from './Arrangement';
import type { Screen } from './geometry';

/** Ring geometry and WebKit painting belong to oink-ui; the app uses CSS decoration. */
describe('canvas selection rings', () => {
  it('uses stacked gap/accent shadows with paired corner declarations', () => {
    const css = readFileSync(join(process.cwd(), 'src/app.css'), 'utf8');
    for (const selector of ['.scr__selection', '.scr__group-border']) {
      const rule = css.slice(css.indexOf(selector + ' {')).split('}')[0];
      expect(rule).toContain('corner-shape: var(--ok-corner); --ok-shape: var(--ok-corner)');
      expect(rule).toContain('box-shadow: 0 0 0 2.5px var(--ok-well), 0 0 0 4.5px var(--ok-accent)');
    }
  });

  describe('rendering', () => {
    const observing = new Set<object>();
    // Assert the lifetime contract before Vitest tears JSDOM down. Without an
    // unmount this fails deterministically, instead of relying on a scheduled
    // React callback winning the race against removal of the global window.
    afterAll(() => {
      try {
        expect(document.querySelectorAll('.arr'), 'a mounted React tree escaped its test').toHaveLength(0);
        expect(observing.size, 'native resize subscriptions escaped their test').toBe(0);
      } finally {
        vi.restoreAllMocks();
        vi.unstubAllGlobals();
      }
    });
    // Vitest globals are disabled, so RTL cannot discover a global afterEach
    // to install automatic cleanup. Unmount before the JSDOM window disappears.
    afterEach(cleanup);
    beforeAll(() => {
      vi.stubGlobal('ResizeObserver', class {
        observe() { observing.add(this); }
        unobserve() { observing.delete(this); }
        disconnect() { observing.delete(this); }
      });
      vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(() => ({ left: 0, top: 0, x: 0, y: 0, width: 900, height: 600, right: 900, bottom: 600, toJSON() {} }));
      vi.stubGlobal('matchMedia', () => ({ matches: false, addEventListener() {}, removeEventListener() {} }));
    });
    const screen = (id: string, x: number, device = id): Screen => ({ id, device, name: id, x, y: 0, w: 1920, h: 1080, wallpaper: 0, groupSize: 1 });
    const settings = { showHotZones: false, snapping: true, compact: false, padding: 24, step: 10 };

    it('mounts one decorated ring for a screen or a computer', () => {
      const one = render(<Arrangement screens={[screen('a', 0), screen('b', 2000)]} selectedId="a" onSelect={() => {}} onScreensChange={() => {}} settings={settings}/>);
      expect(one.container.querySelectorAll('.scr__selection')).toHaveLength(1);
      expect(one.container.querySelector('.scr[data-selected] .scr__selection')).toBeTruthy();
      expect(one.container.querySelector('.scr__ring')).toBeNull();
      one.unmount();
      const pair = [{ ...screen('m', 0, 'pc'), groupSize: 2 }, { ...screen('s', 1920, 'pc'), groupSize: 2 }];
      const group = render(<Arrangement screens={pair} selectedId="m" onSelect={() => {}} onScreensChange={() => {}} settings={settings}/>);
      expect(group.container.querySelectorAll('.scr__group-border')).toHaveLength(1);
      expect(group.container.querySelectorAll('.scr__selection')).toHaveLength(0);
      expect(group.container.querySelector('.scr__ring')).toBeNull();
    });
  });
});
