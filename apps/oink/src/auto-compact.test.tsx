// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { App } from './App';
import { agent } from './api/store';
import type { DeviceCardsView } from './api/cards';
import type { Layout, Placement } from './api/types';
import { cameraStore } from './arrange/cameraStore';
import { setLanguage } from './i18n';
import { profileHarness } from './arrange/profileHarness';

// The contract fixture adopts committed placements exactly; compaction is the editor's.
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

const [A, B] = ['a'.repeat(64), 'b'.repeat(64)];
/** Two computers with one 1920×1080 screen each; B starts 0.2 to the right of A's edge. */
function layout(placements: Placement[] = [{ device: A, origin: { x: 0, y: 0 }, scale: 1 / 1920 }, { device: B, origin: { x: 1.2, y: 0 }, scale: 1 / 1920 }], hidden = false): Layout {
  return { revision: 1, source: 'manual', geometry: { bounds: { x: 0, y: 0, w: 3, h: 1 }, screens: placements.map(p => ({
    key: { device: p.device, display: '0' }, device_name: p.device === A ? 'Alpha' : 'Beta', os: 'macos',
    display: { key: '0', name: 'Display', bounds: { x: 0, y: 0, w: 1920, h: 1080 }, pixel_width: 1920, pixel_height: 1080,
      reference_extent: null, rotation_deg: 0, is_primary: true, is_builtin: false, is_virtual: false },
    origin: p.origin, size: { w: 1920 * p.scale, h: 1080 * p.scale }, size_source: 'manual', gain: 1,
    hidden: hidden && p.device === B, device_disabled: false, online: true,
  })) } };
}
function cards(offline = false): DeviceCardsView {
  return { schema: 1, stream: 'test', revision: '1', group: null, generatedAtMs: 0, exitVolume: null, emergencyDelivery: null,
    cards: [A, B].map(device => ({ device, name: device === A ? 'Alpha' : 'Beta', os: 'macos', logo: 'macos', isSelf: device === A,
      section: device === A ? 'this_device' : offline ? 'offline' : 'online', lastSeenMs: null, status: null, pause: null,
      connection: device === A ? { kind: 'self' } : offline ? { kind: 'offline' } : { kind: 'online', signal: { kind: 'unknown', rttMs: null, bars: 0, colour: 'grey' } } })) } as DeviceCardsView;
}
const x = (placements: Placement[], device: string) => placements.find(p => p.device === device)!.origin.x;
/** Each screen is 1 wide, so this is the opening between A's right and B's left edge. */
const gapOf = (placements: Placement[]) => x(placements, B) - x(placements, A) - 1;

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
  agent.adopt('cards', cards());
  agent.adopt('layout', layout());
  backend.harness = profileHarness({ layout: layout() });
});
afterEach(() => { backend.harness!.stop(); cleanup(); agent.reset(); localStorage.clear(); });

const toggle = () => screen.getByRole('button', { name: 'Auto-compact' });
async function dragBeta(dx: number) {
  const display = document.querySelector('.scr[aria-label^="Beta,"]')!;
  const zoom = cameraStore.get()!.zoom;
  fireEvent.pointerDown(display, { button: 0, clientX: 600, clientY: 300 });
  fireEvent.pointerMove(display, { clientX: 600 + dx * 4096 * zoom, clientY: 300 });
  await act(async () => { fireEvent.pointerUp(display); });
  await backend.harness!.settle();
}

it('sits after the layout profile menu and left of undo and redo, defaults on and persists only on this device', () => {
  const { unmount } = render(<App/>);
  const tools = screen.getByRole('toolbar', { name: 'Arrangement actions' });
  const names = Array.from(tools.querySelectorAll('button'), b => b.getAttribute('aria-label') ?? b.textContent);
  // The current layout profile's menu and + lead the toolbar.
  expect(names).toEqual(['Current layout: Default layout', 'New layout', 'Auto-compact', 'Undo', 'Redo', 'Reset arrangement']);
  expect(toggle().getAttribute('aria-pressed')).toBe('true');
  expect(localStorage.getItem('oink:prefs')).toBeNull();
  fireEvent.click(toggle());
  expect(toggle().getAttribute('aria-pressed')).toBe('false');
  expect(JSON.parse(localStorage.getItem('oink:prefs')!)).toEqual({ compact_layout: false });
  unmount();
  render(<App/>);
  expect(toggle().getAttribute('aria-pressed')).toBe('false');
  fireEvent.click(toggle());
  expect(toggle().getAttribute('aria-pressed')).toBe('true');
  expect(JSON.parse(localStorage.getItem('oink:prefs')!)).toEqual({ compact_layout: true });
});

it('draws two arrows folding in on a dashed centre line (Lucide fold-horizontal)', () => {
  render(<App/>);
  const paths = Array.from(toggle().querySelectorAll('path'), p => p.getAttribute('d')!);
  // Arms from both edges, four dashes on the centre line, and two arrowheads pointing inward.
  expect(paths).toContain('M2 10h5M18 10h-5');
  expect(paths.join(' ').match(/M10 [\d.]+v/g)).toHaveLength(4);
  expect(paths).toContain('M15.5 7.5 13 10l2.5 2.5M4.5 12.5 7 10 4.5 7.5');
  expect(toggle().querySelector('rect')).toBeNull();
});

it('keeps working for this session when storage is unavailable', () => {
  render(<App/>);
  const write = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('blocked'); });
  fireEvent.click(toggle());
  expect(toggle().getAttribute('aria-pressed')).toBe('false');
  write.mockRestore();
});

it('saves the compacted layout for a manual drag as one undo step', async () => {
  render(<App/>);
  await dragBeta(0.1);
  expect(saved()).toHaveLength(1);
  expect(gapOf(saved()[0])).toBeCloseTo(0, 9);
  const undo = screen.getByRole('button', { name: 'Undo' }) as HTMLButtonElement;
  await act(async () => { fireEvent.click(undo); });
  await backend.harness!.settle();
  expect(saved()).toHaveLength(2);
  // Undo replays the exact pre-drag placements, gap included.
  expect(x(saved()[1], A)).toBe(0);
  expect(x(saved()[1], B)).toBeCloseTo(1.2, 12);
  expect(undo.disabled).toBe(true);
});

it('saves the dragged gap unchanged when auto-compaction is off', async () => {
  render(<App/>);
  fireEvent.click(toggle());
  await dragBeta(0.1);
  expect(saved()).toHaveLength(1);
  expect(gapOf(saved()[0])).toBeCloseTo(0.3, 9);
});

it('does not compact or save on hotplug, hidden screens, offline devices or remote layouts', async () => {
  render(<App/>);
  await act(async () => {
    agent.adopt('layout', layout([{ device: A, origin: { x: 0, y: 0 }, scale: 1 / 1920 }])); // unplugged
    agent.adopt('layout', layout()); // plugged back
    agent.adopt('layout', layout(undefined, true)); // hidden
    agent.adopt('layout', layout());
    agent.adopt('cards', cards(true)); // offline
    agent.adopt('cards', cards());
    agent.adopt('layout', layout([{ device: A, origin: { x: 0, y: 0 }, scale: 1 / 1920 }, { device: B, origin: { x: 1.5, y: 0.1 }, scale: 1 / 1920 }])); // remote edit
  });
  expect(saved()).toEqual([]);
});

describe('turning auto-compact on', () => {
  const start = (value: Layout) => { agent.adopt('layout', value); backend.harness!.stop(); backend.harness = profileHarness({ layout: value }); };
  const flip = async () => { await act(async () => { fireEvent.click(toggle()); }); await backend.harness!.settle(); };

  it('compacts once toward this device as one undo step; turning it off changes nothing', async () => {
    render(<App/>);
    await flip(); // off
    expect(toggle().getAttribute('aria-pressed')).toBe('false');
    expect(saved()).toEqual([]);
    await flip(); // on
    expect(toggle().getAttribute('aria-pressed')).toBe('true');
    expect(backend.harness!.of('commit_arrangement').map(r => r.params!.native_plan_tokens)).toEqual([[]]);
    // This device (Alpha) is the anchor; Beta closes its gap.
    expect(x(saved()[0], A)).toBe(0);
    expect(gapOf(saved()[0])).toBeCloseTo(0, 9);
    const undo = screen.getByRole('button', { name: 'Undo' }) as HTMLButtonElement;
    await act(async () => { fireEvent.click(undo); });
    await backend.harness!.settle();
    expect(backend.harness!.of('replay_arrangement').map(r => r.params!.direction)).toEqual(['undo']);
    expect(x(saved()[1], B)).toBeCloseTo(1.2, 12);
    expect(undo.disabled).toBe(true);
    expect(toggle().getAttribute('aria-pressed')).toBe('true');
  });

  it('only turns on when the layout is already compact', async () => {
    start(layout([{ device: A, origin: { x: 0, y: 0 }, scale: 1 / 1920 }, { device: B, origin: { x: 1, y: 0 }, scale: 1 / 1920 }]));
    render(<App/>);
    await flip();
    await flip();
    expect(toggle().getAttribute('aria-pressed')).toBe('true');
    expect(saved()).toEqual([]);
    expect((screen.getByRole('button', { name: 'Undo' }) as HTMLButtonElement).disabled).toBe(true);
  });

  it('ignores hidden screens', async () => {
    start(layout(undefined, true));
    render(<App/>);
    await flip();
    await flip();
    expect(saved()).toEqual([]);
  });
});
