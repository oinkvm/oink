// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { App } from './App';
import { agent } from './api/store';
import type { DeviceCardsView } from './api/cards';
import type { Layout, Placement } from './api/types';
import { profileHarness } from './arrange/profileHarness';
import { setLanguage } from './i18n';

const backend = vi.hoisted(() => ({ harness: null as null | ReturnType<typeof import('./arrange/profileHarness').profileHarness> }));
vi.mock('./api/store', async original => ({ ...await original<typeof import('./api/store')>(), start: vi.fn() }));
vi.mock('./api/transport', async original => ({
  ...await original<typeof import('./api/transport')>(),
  transport: {
    call: (request: { method: string; params?: Record<string, unknown> }) => backend.harness!.call(request),
    shell: async () => null,
    onEvent: async () => () => {},
  },
}));

/**
 * A reported layout (user report 2026-10-21), as canvas rectangles measured from the
 * screenshot (px / 1000): a 3840×2160 computer floats above and right; below it a
 * 1728×1117 computer on the left and this 1728×1117 Mac on the right, with a gap.
 */
const [SELF, LEFT, TOP] = ['a', 'b', 'c'].map(d => d.repeat(64));
const SCREENS = {
  [SELF]: { name: 'This Mac', native: { w: 1728, h: 1117 }, rect: { x: .749, y: .852, w: .491 } },
  [LEFT]: { name: 'Left Mac', native: { w: 1728, h: 1117 }, rect: { x: .127, y: .804, w: .492 } },
  [TOP]: { name: 'Big PC', native: { w: 3840, h: 2160 }, rect: { x: .566, y: .090, w: .722 } },
};
function layout(): Layout {
  const screens = Object.entries(SCREENS).map(([device, s]) => {
    const scale = s.rect.w / s.native.w;
    return { key: { device, display: '0' }, device_name: s.name, os: 'macos' as const,
      display: { key: '0', name: 'Display', bounds: { x: 0, y: 0, ...s.native }, pixel_width: s.native.w, pixel_height: s.native.h,
        reference_extent: null, rotation_deg: 0, is_primary: true, is_builtin: false, is_virtual: false },
      origin: { x: s.rect.x, y: s.rect.y }, size: { w: s.rect.w, h: s.native.h * scale }, size_source: 'manual' as const, gain: 1,
      hidden: false, device_disabled: false, online: true };
  });
  return { revision: 1, source: 'manual', geometry: { bounds: { x: 0, y: 0, w: 1.3, h: 1.2 }, screens } };
}
const cards = { schema: 1, stream: 'test', revision: '1', group: null, generatedAtMs: 0, exitVolume: null, emergencyDelivery: null,
  cards: Object.entries(SCREENS).map(([device, s]) => ({ device, name: s.name, os: 'macos', logo: 'macos', isSelf: device === SELF,
    section: device === SELF ? 'this_device' : 'online', lastSeenMs: null, status: null, pause: null,
    connection: device === SELF ? { kind: 'self' } : { kind: 'online', signal: { kind: 'unknown', rttMs: null, bars: 0, colour: 'grey' } } })) } as DeviceCardsView;

beforeAll(() => {
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  vi.stubGlobal('PointerEvent', MouseEvent);
  HTMLElement.prototype.setPointerCapture = () => {};
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(() => ({ left: 0, top: 0, x: 0, y: 0, width: 900, height: 600, right: 900, bottom: 600, toJSON() {} }));
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
beforeEach(() => {
  setLanguage('en');
  agent.adopt('boot', { platform: 'macos', version: '0.1.0', build: null, backend: 'agent' });
  agent.adopt('cards', cards);
  agent.adopt('layout', layout());
  backend.harness = profileHarness({ layout: layout() });
});
afterEach(() => { backend.harness!.stop(); cleanup(); agent.reset(); localStorage.clear(); });

const toggle = () => screen.getByRole('button', { name: 'Auto-compact' });
const flip = async () => { await act(async () => { fireEvent.click(toggle()); }); await backend.harness!.settle(); };
const origin = (placements: Placement[], device: string) => placements.find(p => p.device === device)!.origin;

it('moves the floating computer straight down onto the row below when auto-compact is turned on', async () => {
  render(<App/>);
  await flip(); // off
  await flip(); // on
  const commits = backend.harness!.of('commit_arrangement').map(r => r.params!.placements as Placement[]);
  expect(commits).toHaveLength(1);
  // It stops on the left computer's top edge: 0.804 − 0.406.
  expect(origin(commits[0], TOP).x).toBeCloseTo(.566, 9);
  expect(origin(commits[0], TOP).y + .722 * 2160 / 3840).toBeCloseTo(.804, 9);
  expect(origin(commits[0], SELF)).toEqual({ x: .749, y: .852 });
  expect(origin(commits[0], LEFT)).toEqual({ x: .127, y: .804 });
});

it('compacts on the first turn-on even when the stored preference was off', async () => {
  localStorage.setItem('oink:prefs', JSON.stringify({ compact_layout: false }));
  render(<App/>);
  expect(toggle().getAttribute('aria-pressed')).toBe('false');
  await flip();
  expect(backend.harness!.of('commit_arrangement')).toHaveLength(1);
});
