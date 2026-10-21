// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { App } from './App';
import { agent } from './api/store';
import type { DeviceCardsView } from './api/cards';
import type { Layout, Placement } from './api/types';
import { cameraStore } from './arrange/cameraStore';
import { threeComputers } from './arrange/threeComputers.fixture';
import { setLanguage } from './i18n';

const saved = vi.hoisted(() => [] as Placement[][]);
vi.mock('./api/store', async original => ({ ...await original<typeof import('./api/store')>(), start: vi.fn() }));
vi.mock('./api/transport', async original => ({
  ...await original<typeof import('./api/transport')>(),
  transport: {
    // The backend adopts saved placements exactly.
    call: async ({ method, params }: { method: string; params?: { placements: Placement[] } }) => {
      if (method === 'save_layout') { saved.push(params!.placements); return place(params!.placements); }
      return method === 'group' ? { members: [] } : [];
    },
    shell: async () => null,
    onEvent: async () => () => {},
  },
}));

const [A, B, C] = ['a', 'b', 'c'].map(d => d.repeat(64));
function place(placements: Placement[]): Layout {
  return { ...threeComputers, geometry: { ...threeComputers.geometry, screens: threeComputers.geometry.screens.map(s => {
    const p = placements.find(p => p.device === s.key.device)!, n = s.display.bounds;
    return { ...s, origin: { x: p.origin.x + n.x * p.scale, y: p.origin.y + n.y * p.scale }, size: { w: n.w * p.scale, h: n.h * p.scale } };
  }) } };
}
const cards = { schema: 1, stream: 'test', revision: '1', group: null, generatedAtMs: 0, exitVolume: null, emergencyDelivery: null,
  cards: [A, B, C].map(device => ({ device, name: device === C ? 'This Mac' : device === B ? 'Other Mac' : 'PC', os: 'macos', logo: 'macos',
    isSelf: device === C, section: device === C ? 'this_device' : 'online', lastSeenMs: null, status: null, pause: null,
    connection: device === C ? { kind: 'self' } : { kind: 'online', signal: { kind: 'unknown', rttMs: null, bars: 0, colour: 'grey' } } })) } as DeviceCardsView;
const origin = (placements: Placement[], device: string) => placements.find(p => p.device === device)!.origin;
const screenOf = (device: string, display: string) => threeComputers.geometry.screens.find(s => s.key.device === device && s.key.display === display)!;

beforeAll(() => {
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  vi.stubGlobal('PointerEvent', MouseEvent);
  HTMLElement.prototype.setPointerCapture = () => {};
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(() => ({ left: 0, top: 0, x: 0, y: 0, width: 900, height: 600, right: 900, bottom: 600, toJSON() {} }));
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
beforeEach(() => {
  setLanguage('en');
  saved.length = 0;
  agent.adopt('boot', { platform: 'macos', version: '0.1.0', build: null, backend: 'agent' });
  agent.adopt('cards', cards);
  agent.adopt('layout', threeComputers);
});
afterEach(() => { cleanup(); agent.reset(); localStorage.clear(); });

it('a far bottom-right release of this Mac stops at the other Mac\'s bounding rectangle, as one undo step', async () => {
  render(<App/>);
  // The hidden and disabled PC screen is not drawn.
  expect(document.querySelectorAll('.scr')).toHaveLength(3);
  const display = document.querySelector('.scr[aria-label^="This Mac,"]')!;
  const zoom = cameraStore.get()!.zoom;
  fireEvent.pointerDown(display, { button: 0, clientX: 600, clientY: 300 });
  // About 0.9 to the right and 0.7 below its stored place, clear of everything.
  fireEvent.pointerMove(display, { clientX: 600 + 0.9 * 4096 * zoom, clientY: 300 + 0.7 * 4096 * zoom });
  await act(async () => { fireEvent.pointerUp(display); });
  expect(saved).toHaveLength(1);
  const builtIn = screenOf(B, '0'), external = screenOf(B, '1');
  const c = origin(saved[0], C);
  expect(c.x).toBeCloseTo(external.origin.x + external.size.w, 9);
  expect(c.y).toBeCloseTo(builtIn.origin.y + builtIn.size.h, 9);
  // Only the edited computer moves; the hidden PC keeps its place too.
  expect(origin(saved[0], B)).toEqual(builtIn.origin);
  expect(origin(saved[0], A)).toEqual(screenOf(A, '0').origin);
  const undo = screen.getByRole('button', { name: 'Undo' }) as HTMLButtonElement;
  await act(async () => { fireEvent.click(undo); });
  expect(origin(saved[1], C)).toEqual(screenOf(C, '0').origin);
  expect(undo.disabled).toBe(true);
});
