// @vitest-environment jsdom
import { act, cleanup, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { App } from './App';
import { agent } from './api/store';
import type { DeviceCardsView } from './api/cards';
import type { DeviceArrangementState, PreviewReply } from './api/layoutProfiles';
import { layoutProfilesFixtureSnapshot } from './api/layoutProfiles.fixture';
import type { Placement } from './api/types';
import { cameraStore } from './arrange/cameraStore';
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

// Fixture computers (canvas units, scale 0.001): Alpha has a 1200×800 main screen
// with an 800×600 side screen to its right, top-aligned; Beta is the same at x 2.5;
// Gamma has one screen at x 5.
const [A, B, C] = ['a', 'b', 'c'].map(d => d.repeat(64));
const NAMES: Record<string, string> = { [A]: 'Alpha', [B]: 'Beta', [C]: 'Gamma' };
const cards = { schema: 1, stream: 'test', revision: '1', group: null, generatedAtMs: 0, exitVolume: null, emergencyDelivery: null,
  cards: [A, B, C].map(device => ({ device, name: NAMES[device], os: 'macos', logo: 'macos', isSelf: device === A,
    section: device === A ? 'this_device' : 'online', lastSeenMs: null, status: null, pause: null,
    connection: device === A ? { kind: 'self' } : { kind: 'online', signal: { kind: 'unknown', rttMs: null, bars: 0, colour: 'grey' } } })) } as DeviceCardsView;
const devices = (sync: boolean): DeviceArrangementState[] => [A, B, C].map(device => ({
  device, sync_system_arrangement: sync && device === A, preference_token: `preference:${device}:1`,
  native_token: `native:${device}:1`, availability: 'ready', can_edit_screens: sync && device === A,
}));

beforeAll(() => {
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  vi.stubGlobal('PointerEvent', MouseEvent);
  HTMLElement.prototype.setPointerCapture = () => {};
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(() => ({ left: 0, top: 0, x: 0, y: 0, width: 900, height: 600, right: 900, bottom: 600, toJSON() {} }));
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
beforeEach(() => {
  setLanguage('en');
  // Compaction off: these tests isolate snapping.
  localStorage.setItem('oink:prefs', JSON.stringify({ compact_layout: false }));
  agent.adopt('boot', { platform: 'macos', version: '0.1.0', build: null, backend: 'agent' });
  agent.adopt('cards', cards);
});
afterEach(() => { backend.harness?.stop(); cleanup(); agent.reset(); localStorage.clear(); });

function setup(sync: boolean) {
  const h = backend.harness = profileHarness({ snapshot: layoutProfilesFixtureSnapshot(), devices: devices(sync) });
  agent.adopt('layout', h.fixture.layout());
  render(<App/>);
  return h;
}
const of = (name: string) => Array.from(document.querySelectorAll<HTMLElement>('.scr:not(.scr--group)')).filter(s => s.getAttribute('aria-label')!.startsWith(`${name},`));
async function drag(el: HTMLElement, dx: number, dy: number, select = false) {
  if (select) { fireEvent.pointerDown(el, { button: 0, clientX: 600, clientY: 300 }); fireEvent.pointerUp(el); }
  const zoom = cameraStore.get()!.zoom;
  fireEvent.pointerDown(el, { button: 0, clientX: 600, clientY: 300 });
  fireEvent.pointerMove(el, { clientX: 600 + dx * 4096 * zoom, clientY: 300 + dy * 4096 * zoom });
  await act(async () => { fireEvent.pointerUp(el); });
  await backend.harness!.settle();
}

describe('snapping while dragging', () => {
  it('snaps one screen of a system-arranged computer to its sibling\'s edges', async () => {
    const h = setup(true);
    const [, side] = of('Alpha');
    // 190 down: the side screen's bottom (600 tall) lands 10 short of the main screen's bottom (800).
    await drag(side, 0, .19, true);
    const previews = h.of('preview_system_arrangement');
    expect((previews.at(-1)!.params!.desired_canvas_origin as { y: number }).y).toBeCloseTo(.2, 9);
    const last = previews.at(-1)!.reply as PreviewReply;
    expect(last.native.screens.find(s => s.screen.display === 'side')!.origin).toEqual({ x: 1200, y: 200 });
    expect(h.of('commit_arrangement')[0].params!.native_plan_tokens).toEqual([last.plan_token]);
  });

  it('snaps one screen of a system-arranged computer to another computer\'s edge', async () => {
    const h = setup(true);
    const [, side] = of('Alpha');
    // Beta's main screen starts at x 2.5; the side screen's right edge ends at 2.0.
    await drag(side, .48, 0, true);
    const desired = h.of('preview_system_arrangement').at(-1)!.params!.desired_canvas_origin as { x: number };
    expect(desired.x + .8).toBeCloseTo(2.5, 9);
  });

  it('keeps raw positions while Shift suppresses snapping for a single screen', async () => {
    const h = setup(true);
    const [, side] = of('Alpha');
    fireEvent.pointerDown(side, { button: 0, clientX: 600, clientY: 300 }); fireEvent.pointerUp(side);
    const zoom = cameraStore.get()!.zoom;
    fireEvent.pointerDown(side, { button: 0, clientX: 600, clientY: 300, shiftKey: true });
    fireEvent.pointerMove(side, { clientX: 600, clientY: 300 + .19 * 4096 * zoom, shiftKey: true });
    await act(async () => { fireEvent.pointerUp(side); });
    await h.settle();
    expect((h.of('preview_system_arrangement').at(-1)!.params!.desired_canvas_origin as { y: number }).y).toBeCloseTo(.19, 9);
  });

  it('snaps a whole computer as one rigid group when its system arrangement is off', async () => {
    const h = setup(false);
    const [main] = of('Alpha');
    // Alpha spans x 0–2.0; 0.48 right leaves 0.02 to Beta at 2.5, inside the snap distance.
    await drag(main, .48, 0);
    expect(h.of('preview_system_arrangement')).toHaveLength(0);
    const alpha = (h.of('commit_arrangement')[0].params!.placements as Placement[]).find(p => p.device === A)!;
    expect(alpha.origin.x + 2.0).toBeCloseTo(2.5, 9);
  });
});
