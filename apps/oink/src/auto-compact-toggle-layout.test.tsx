// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { App } from './App';
import { agent } from './api/store';
import type { DeviceCardsView } from './api/cards';
import type { Layout } from './api/types';
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
it('keeps the floating computer and the row unchanged when auto-compact is turned on', async () => {
  render(<App/>);
  const before = structuredClone(backend.harness!.fixture.state().snapshot);
  await flip(); await flip();
  expect(backend.harness!.of('commit_arrangement')).toEqual([]);
  expect(backend.harness!.fixture.state().snapshot).toEqual(before);
  expect(screen.getByRole('button', { name: 'Undo' }).hasAttribute('disabled')).toBe(true);
});

it('only updates the preference on the first turn-on when the stored preference was off', async () => {
  localStorage.setItem('oink:prefs', JSON.stringify({ compact_layout: false }));
  render(<App/>);
  expect(toggle().getAttribute('aria-pressed')).toBe('false');
  await flip();
  expect(toggle().getAttribute('aria-pressed')).toBe('true');
  expect(backend.harness!.of('commit_arrangement')).toEqual([]);
});

it.each([{ sync: false, editable: false }, { sync: true, editable: true }, { sync: true, editable: false }])('leaves multi-screen geometry unchanged on turn-on with sync=$sync editable=$editable', async ({ sync, editable }) => {
  backend.harness!.stop();
  const initial = structuredClone(backend.harness!.fixture.state().snapshot!);
  const [a, b] = initial.native;
  initial.native = [
    { ...a, primary: { device: a.device, display: 'main' }, screens: [
      { screen: { device: a.device, display: 'main' }, origin: { x: 0, y: 0 }, size: { w: 10, h: 10 } },
      { screen: { device: a.device, display: 'side' }, origin: { x: 10, y: -10 }, size: { w: 10, h: 40 } },
    ] },
    { ...b, screens: [{ screen: b.primary, origin: { x: 0, y: 0 }, size: { w: 10, h: 10 } }] },
  ];
  initial.placements = [{ device: a.device, origin: { x: 0, y: 0 }, scale: .001 }, { device: b.device, origin: { x: .01, y: .1 }, scale: .001 }];
  initial.disabled = initial.native.flatMap(n => n.screens.map(s => ({ screen: s.screen, disabled: false })));
  const h = backend.harness = profileHarness({ snapshot: initial, devices: initial.native.map(n => ({ device: n.device,
    sync_system_arrangement: sync && n.device === a.device, can_edit_screens: editable && n.device === a.device,
    native_token: n.token, preference_token: 'preference', availability: 'ready' })) });
  agent.adopt('layout', h.fixture.layout());
  render(<App/>);
  await flip(); await flip();
  const previews = h.of('preview_system_arrangement');
  expect(previews).toHaveLength(0);
  expect(h.of('commit_arrangement')).toEqual([]);
  expect(h.fixture.state().snapshot!.placements).toEqual(initial.placements);
  expect(h.fixture.state().snapshot!.native.map(n => n.screens)).toEqual(initial.native.map(n => n.screens));
  expect(screen.getByRole('button', { name: 'Undo' }).hasAttribute('disabled')).toBe(true);
});
