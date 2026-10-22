// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { App } from './App';
import { agent } from './api/store';
import type { DeviceCardsView } from './api/cards';
import type { Placement } from './api/types';
import { cameraStore } from './arrange/cameraStore';
import { threeComputers } from './arrange/threeComputers.fixture';
import { setLanguage } from './i18n';
import { profileHarness } from './arrange/profileHarness';

// The contract fixture adopts committed placements exactly.
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
const saved = () => backend.harness!.placements();

const [A, B, C] = ['a', 'b', 'c'].map(d => d.repeat(64));
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
  agent.adopt('boot', { platform: 'macos', version: '0.1.0', build: null, backend: 'agent' });
  agent.adopt('cards', cards);
  agent.adopt('layout', threeComputers);
  backend.harness = profileHarness({ layout: threeComputers });
});
afterEach(() => { backend.harness!.stop(); cleanup(); agent.reset(); localStorage.clear(); });

it('a projection-free release reaches the other visible computer bounding corner, as one undo step', async () => {
  render(<App/>);
  // The hidden and disabled PC screen is not drawn.
  expect(document.querySelectorAll('.scr')).toHaveLength(3);
  const display = document.querySelector('.scr[aria-label^="This Mac,"]')!;
  const zoom = cameraStore.get()!.zoom;
  fireEvent.pointerDown(display, { button: 0, clientX: 600, clientY: 300 });
  // About 0.9 to the right and 0.7 below its stored place, clear of everything.
  fireEvent.pointerMove(display, { clientX: 600 + 0.9 * 4096 * zoom, clientY: 300 + 0.7 * 4096 * zoom });
  await act(async () => { fireEvent.pointerUp(display); });
  await backend.harness!.settle();
  expect(saved()).toHaveLength(1);
  const builtIn = screenOf(B, '0');
  const c = origin(saved()[0], C);
  const external = screenOf(B, '1');
  const scale = builtIn.size.w / builtIn.display.bounds.w;
  // Expanding the native geometry and its bounding rectangle can differ by one
  // double ULP; this precision is much tighter than the fixed compaction grid.
  expect(c.x).toBeCloseTo(builtIn.origin.x + external.display.bounds.x * scale + external.display.bounds.w * scale, 14);
  expect(c.y).toBeCloseTo(builtIn.origin.y + builtIn.display.bounds.h * scale, 14);
  // Only the edited computer moves; the hidden PC keeps its place too.
  expect(origin(saved()[0], B)).toEqual(builtIn.origin);
  expect(origin(saved()[0], A)).toEqual(screenOf(A, '0').origin);
  const undo = screen.getByRole('button', { name: 'Undo' }) as HTMLButtonElement;
  await act(async () => { fireEvent.click(undo); });
  await backend.harness!.settle();
  expect(origin(saved()[1], C)).toEqual(screenOf(C, '0').origin);
  expect(undo.disabled).toBe(true);
});
