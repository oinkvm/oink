// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { App } from './App';
import { agent } from './api/store';
import type { DeviceCardsView } from './api/cards';
import type { DeviceArrangementState } from './api/layoutProfiles';
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

// Fixture: Alpha and Beta have a main and a side screen each, Gamma one screen.
const [A, B, C] = ['a', 'b', 'c'].map(d => d.repeat(64));
const NAMES: Record<string, string> = { [A]: 'Alpha', [B]: 'Beta', [C]: 'Gamma' };
const cards = { schema: 1, stream: 'test', revision: '1', group: null, generatedAtMs: 0, exitVolume: null, emergencyDelivery: null,
  cards: [A, B, C].map(device => ({ device, name: NAMES[device], os: 'macos', logo: 'macos', isSelf: device === A,
    section: device === A ? 'this_device' : 'online', lastSeenMs: null, status: null, pause: null,
    connection: device === A ? { kind: 'self' } : { kind: 'online', signal: { kind: 'unknown', rttMs: null, bars: 0, colour: 'grey' } } })) } as DeviceCardsView;
/** Alpha writes its system arrangement when `sync` is on. */
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
  agent.adopt('boot', { platform: 'macos', version: '0.1.0', build: null, backend: 'agent' });
  agent.adopt('cards', cards);
});
afterEach(() => { backend.harness?.stop(); cleanup(); agent.reset(); localStorage.clear(); });

function setup(sync = false, compact = true) {
  if (!compact) localStorage.setItem('oink:prefs', JSON.stringify({ compact_layout: false }));
  const h = backend.harness = profileHarness({ snapshot: layoutProfilesFixtureSnapshot(), devices: devices(sync) });
  agent.adopt('layout', h.fixture.layout());
  render(<App/>);
  return h;
}
/** Screens in layout order: main first, then side. */
const of = (name: string) => Array.from(document.querySelectorAll<HTMLElement>('.scr:not(.scr--group)')).filter(s => s.getAttribute('aria-label')!.startsWith(`${name},`));
const pressed = () => Array.from(document.querySelectorAll<HTMLElement>('.scr:not(.scr--group)')).filter(s => s.getAttribute('aria-pressed') === 'true').map(s => s.getAttribute('aria-label')!.split(',')[0] + (of(s.getAttribute('aria-label')!.split(',')[0]).indexOf(s) ? ':side' : ':main'));
const click = (el: HTMLElement, init: Partial<PointerEventInit> = {}) => { fireEvent.pointerDown(el, { button: 0, clientX: 600, clientY: 300, ...init }); fireEvent.pointerUp(el, init); };
const cmdClick = (el: HTMLElement) => click(el, { metaKey: true });
const placement = (placements: Placement[], device: string) => placements.find(p => p.device === device)!;
async function drag(el: Element, dx: number, dy = 0) {
  const zoom = cameraStore.get()!.zoom;
  fireEvent.pointerDown(el, { button: 0, clientX: 600, clientY: 300 });
  fireEvent.pointerMove(el, { clientX: 600 + dx * 4096 * zoom, clientY: 300 + dy * 4096 * zoom });
  await act(async () => { fireEvent.pointerUp(el); });
  await backend.harness!.settle();
}
const commits = () => backend.harness!.of('commit_arrangement').map(r => r.params!);
const before = (device: string) => placement(layoutProfilesFixtureSnapshot().placements, device);

describe('selecting screens', () => {
  it('keeps the newly clicked screen of a system-arranged computer selected (bug 1)', () => {
    setup(true);
    const [main, side] = of('Alpha');
    click(side);
    expect(pressed()).toEqual(['Alpha:side']);
    click(main);
    expect(pressed()).toEqual(['Alpha:main']);
  });

  it('selects the whole computer from one screen when its system arrangement is off', () => {
    setup(false);
    click(of('Alpha')[1]);
    expect(pressed()).toEqual(['Alpha:main', 'Alpha:side']);
    const box = document.querySelector<HTMLElement>(`.scr--group[data-group="${A}"]`)!;
    expect(box.hasAttribute('data-screen-selected')).toBe(false);
  });

  it('selects every screen of a computer on double-click, even with system arrangement on', () => {
    setup(true);
    const [, side] = of('Alpha');
    click(side);
    fireEvent.doubleClick(side);
    expect(pressed()).toEqual(['Alpha:main', 'Alpha:side']);
    expect(document.querySelector(`.scr--group[data-group="${A}"]`)!.hasAttribute('data-screen-selected')).toBe(false);
  });

  it('adds and removes computers with Cmd+click, always as whole computers', () => {
    setup(true);
    click(of('Gamma')[0]);
    cmdClick(of('Beta')[1]);
    expect(pressed()).toEqual(['Beta:main', 'Beta:side', 'Gamma:main']);
    // A screen of a system-arranged computer joins with its whole computer too.
    cmdClick(of('Alpha')[1]);
    expect(pressed()).toEqual(['Alpha:main', 'Alpha:side', 'Beta:main', 'Beta:side', 'Gamma:main']);
    cmdClick(of('Beta')[0]);
    expect(pressed()).toEqual(['Alpha:main', 'Alpha:side', 'Gamma:main']);
    cmdClick(of('Gamma')[0]);
    cmdClick(of('Alpha')[0]);
    expect(pressed()).toEqual([]);
  });

  it('does not let the focus from a Cmd+click press re-select the screen alone', () => {
    setup(false);
    click(of('Gamma')[0]);
    const beta = of('Beta')[0];
    fireEvent.pointerDown(beta, { button: 0, clientX: 600, clientY: 300, metaKey: true });
    fireEvent.focus(beta); // a browser focuses the pressed screen here
    fireEvent.pointerUp(beta, { metaKey: true });
    expect(pressed()).toEqual(['Beta:main', 'Beta:side', 'Gamma:main']);
    // Keyboard focus still selects.
    fireEvent.focus(of('Alpha')[0]);
    expect(pressed()).toEqual(['Alpha:main', 'Alpha:side']);
  });

  it('widens a single system-arranged screen to its computer when another of its screens is Cmd+clicked', () => {
    setup(true);
    const [main, side] = of('Alpha');
    click(side);
    cmdClick(main);
    expect(pressed()).toEqual(['Alpha:main', 'Alpha:side']);
  });

  it('draws one solid bounding rectangle around a multi-computer group', () => {
    setup(false);
    click(of('Beta')[0]);
    cmdClick(of('Gamma')[0]);
    const boxes = document.querySelectorAll<HTMLElement>('.scr--group');
    expect(Array.from(boxes, b => b.dataset.group)).toEqual(['selection']);
    expect(boxes[0].hasAttribute('data-screen-selected')).toBe(false);
    // Members show no rings of their own; the group box carries the handles.
    expect(of('Gamma')[0].hasAttribute('data-selected')).toBe(false);
    expect(boxes[0].querySelectorAll('.scr__hz')).toHaveLength(4);
  });
});

describe('group outlines', () => {
  const outlines = () => Array.from(document.querySelectorAll<HTMLElement>('.scr-outline'), o => o.dataset.outline);

  it('outlines a computer without system arrangement as one big screen inside a group', () => {
    setup(false);
    click(of('Beta')[0]);
    cmdClick(of('Gamma')[0]);
    expect(outlines()).toEqual([B, C]);
    expect(document.querySelectorAll('.scr--group[data-group="selection"]')).toHaveLength(1);
  });

  it('outlines each screen of a system-arranged computer inside a group', () => {
    setup(true);
    click(of('Gamma')[0]);
    cmdClick(of('Alpha')[0]);
    expect(outlines()).toEqual([`${A}:main`, `${A}:side`, C]);
  });

  it('shows only the single solid ring around one selected computer', () => {
    setup(false);
    click(of('Beta')[1]);
    expect(outlines()).toEqual([]);
    expect(Array.from(document.querySelectorAll<HTMLElement>('.scr--group'), g => g.dataset.group)).toEqual([B]);
    expect(of('Beta').some(s => s.hasAttribute('data-selected'))).toBe(false);
  });

  it('keeps the dashed computer box and solid screen for one system-arranged screen', () => {
    setup(true);
    click(of('Alpha')[1]);
    expect(outlines()).toEqual([]);
    expect(document.querySelector(`.scr--group[data-group="${A}"]`)!.hasAttribute('data-screen-selected')).toBe(true);
    expect(of('Alpha')[1].hasAttribute('data-selected')).toBe(true);
  });
});

describe('editing a selected group', () => {
  it('moves the whole group as one history entry of whole-computer translations', async () => {
    const h = setup(false, false);
    click(of('Beta')[0]);
    cmdClick(of('Gamma')[0]);
    await drag(of('Gamma')[0], 0.5, 0.25);
    expect(commits()).toHaveLength(1);
    const [commit] = commits();
    expect(commit.native_plan_tokens).toEqual([]);
    const placements = commit.placements as Placement[];
    for (const device of [B, C]) {
      expect(placement(placements, device).origin.x - before(device).origin.x).toBeCloseTo(0.5, 9);
      expect(placement(placements, device).origin.y - before(device).origin.y).toBeCloseTo(0.25, 9);
      expect(placement(placements, device).scale).toBe(before(device).scale);
    }
    expect(placement(placements, A)).toEqual(before(A));
    const undo = screen.getByRole('button', { name: 'Undo' }) as HTMLButtonElement;
    await act(async () => { fireEvent.click(undo); });
    await h.settle();
    expect(h.of('replay_arrangement')).toHaveLength(1);
    expect(undo.disabled).toBe(true);
  });

  it('scales the whole group uniformly from its bounding rectangle handle', async () => {
    setup(false, false);
    click(of('Beta')[0]);
    cmdClick(of('Gamma')[0]);
    const handle = document.querySelector<HTMLElement>('.scr--group[data-group="selection"] .scr__hz[data-c="se"]')!;
    const zoom = cameraStore.get()!.zoom;
    fireEvent.pointerDown(handle, { button: 0, clientX: 600, clientY: 300 });
    fireEvent.pointerMove(handle, { clientX: 600 + 1.5 * 4096 * zoom, clientY: 300 });
    await act(async () => { fireEvent.pointerUp(handle); });
    await backend.harness!.settle();
    expect(commits()).toHaveLength(1);
    const placements = commits()[0].placements as Placement[];
    const k = placement(placements, B).scale / before(B).scale;
    expect(k).toBeGreaterThan(1);
    expect(placement(placements, C).scale / before(C).scale).toBeCloseTo(k, 9);
    // The anchor opposite the dragged corner (the group's top-left) stays put.
    expect(placement(placements, B).origin.x).toBeCloseTo(before(B).origin.x, 9);
    expect(placement(placements, C).origin.x - placement(placements, B).origin.x).toBeCloseTo((before(C).origin.x - before(B).origin.x) * k, 9);
    expect(placement(placements, A)).toEqual(before(A));
  });

  it('compacts each moved unit once, keeping a balanced middle unit in place', async () => {
    setup(false, true);
    click(of('Beta')[0]);
    cmdClick(of('Gamma')[0]);
    await drag(of('Gamma')[0], 1.5);
    const placements = commits()[0].placements as Placement[];
    // Beta faces one unit on each side: its sum is constant, so the tie-break
    // keeps its released position. Gamma then closes its gap to Beta.
    expect(placement(placements, B).origin.x).toBeCloseTo(before(B).origin.x + 1.5, 9);
    expect(placement(placements, C).origin.x - placement(placements, B).origin.x).toBeLessThan(before(C).origin.x - before(B).origin.x);
    expect(placement(placements, A)).toEqual(before(A));
  });

  it('nudges the whole group with the arrow keys', async () => {
    setup(false, false);
    click(of('Beta')[0]);
    cmdClick(of('Gamma')[0]);
    await act(async () => { fireEvent.keyDown(document.body, { key: 'ArrowDown' }); });
    await backend.harness!.settle();
    expect(commits()).toHaveLength(1);
    const placements = commits()[0].placements as Placement[];
    const dy = placement(placements, B).origin.y - before(B).origin.y;
    expect(dy).toBeGreaterThan(0);
    expect(placement(placements, C).origin.y - before(C).origin.y).toBeCloseTo(dy, 12);
    expect(placement(placements, A)).toEqual(before(A));
  });
});

it('keeps sidebar and canvas selection linked and clears both on sidebar blank space', () => {
  setup(false);
  const row=document.querySelector<HTMLElement>(`[data-ctx="device:${A}"] .dev__hit`)!;
  fireEvent.click(row);
  expect(pressed()).toEqual(['Alpha:main','Alpha:side']);
  click(of('Beta')[0]);
  expect(document.querySelector(`[data-ctx="device:${B}"]`)?.hasAttribute('data-current')).toBe(true);
  fireEvent.click(document.querySelector('.win__side')!);
  expect(pressed()).toEqual([]);
  expect(document.querySelector('.dev[data-current]')).toBeNull();
});

it('treats an offline sync-on desktop as rigid and preserves the moved placement on reconnect', async () => {
  const h = setup(true, false);
  await act(async () => { h.fixture.setOnline(A, false); });
  const nativeBefore = structuredClone(h.fixture.state().snapshot!.native);
  click(of('Alpha')[1]);
  expect(pressed()).toEqual(['Alpha:main', 'Alpha:side']);
  await drag(of('Alpha')[1], -.3, .2);
  expect(h.of('preview_system_arrangement')).toHaveLength(0);
  expect(commits()).toHaveLength(1);
  expect(commits()[0].native_plan_tokens).toEqual([]);
  expect(h.fixture.state().snapshot!.native.map(n => n.screens)).toEqual(nativeBefore.map(n => n.screens));
  const moved = structuredClone(h.fixture.state().snapshot!.placements);
  expect(placement(moved, A).origin).not.toEqual(before(A).origin);
  await act(async () => { h.fixture.setOnline(A, true); });
  await h.settle();
  expect(h.fixture.state().snapshot!.placements).toEqual(moved);
  expect(h.fixture.state().snapshot!.native.map(n => n.screens)).toEqual(nativeBefore.map(n => n.screens));
  expect(commits()).toHaveLength(1);
});
