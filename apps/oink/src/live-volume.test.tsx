// @vitest-environment jsdom
import { act, cleanup, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { App } from './App';
import { agent } from './api/store';
import type { DeviceCardsView } from './api/cards';
import { layoutProfilesFixtureSnapshot } from './api/layoutProfiles.fixture';
import { profileHarness } from './arrange/profileHarness';
import { setLanguage } from './i18n';

const backend = vi.hoisted(() => ({
  harness: null as null | ReturnType<typeof import('./arrange/profileHarness').profileHarness>,
  exit: [] as { versionToken: string; volumePercent: number; muted: boolean }[],
  owner: null as null | ((volume: number, muted: boolean) => void),
}));
vi.mock('./api/store', async original => ({ ...await original<typeof import('./api/store')>(), start: vi.fn() }));
vi.mock('./api/transport', async original => ({
  ...await original<typeof import('./api/transport')>(),
  transport: {
    call: async (request: { method: string; params?: Record<string, unknown> }) => {
      if (request.method === 'set_exit_volume') {
        const p = request.params as { requestId: string; versionToken: string; volumePercent: number; muted: boolean };
        backend.exit.push({ versionToken: p.versionToken, volumePercent: p.volumePercent, muted: p.muted });
        // The exit owner applies, then its next card snapshot carries the new revision.
        queueMicrotask(() => backend.owner!(p.volumePercent, p.muted));
        return { type: 'device_control_completed', data: { requestId: p.requestId, result: { outcome: 'applied' } } };
      }
      return backend.harness!.call(request);
    },
    shell: async () => null,
    onEvent: async () => () => {},
  },
}));

const A = 'a'.repeat(64);
const action = { kind: 'invoke', command: 'x', token: 'x' } as const;
const resource = { configuredMode: 'shared', icon: 'shared', fault: { code: 'none', unknownCode: null, problem: null }, condition: 'ready', action } as const;
function cards(revision: number, volumePercent: number, muted = false): DeviceCardsView {
  return {
    schema: 1, stream: 'live', revision: String(revision), group: null, generatedAtMs: 0, emergencyDelivery: null,
    exitVolume: { receiverDevice: A, receiverName: 'Alpha', outputName: 'Speakers', volumePercent, muted, condition: 'ready', versionToken: `v${revision}`, canSet: true, refusal: null },
    cards: [{
      device: A, name: 'Alpha', os: 'macos', logo: 'macos', isSelf: true, section: 'this_device', lastSeenMs: null, pause: null, connection: { kind: 'self' },
      status: {
        actions: { screenShared: action, screenLocal: action, mouseShared: action, mouseLocal: action, keyboardShared: action, keyboardLocal: action, keyboardDisabled: action, keyboardEnabled: action, clipboardShared: action, clipboardLocal: action, forwardingEnabled: action, forwardingDisabled: action },
        cursor: { icon: 'local', focusDevice: A, focusName: 'Alpha', lockedDevice: null, context: 'group' },
        screen: resource, mouse: resource, keyboard: { ...resource, disabled: false }, clipboard: resource,
        speaker: { ...resource, configuredMode: 'receiver', icon: 'exit', makeExitAction: action, forwardingControlsToken: null, forwardingEnabled: null, activity: 'playing', receiverDevice: A, receiverName: 'Alpha', activeSources: 1, receiveReady: true, forwarding: null },
        game: { preference: 'auto', active: false, detected: false, actions: { auto: action, on: action, off: action } },
      },
    }],
  } as unknown as DeviceCardsView;
}

beforeAll(() => {
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  vi.stubGlobal('PointerEvent', MouseEvent);
  HTMLElement.prototype.setPointerCapture = () => {};
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
beforeEach(() => {
  setLanguage('en');
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] });
  agent.adopt('boot', { platform: 'macos', version: '0.1.0', build: null, backend: 'agent' });
  let revision = 1;
  agent.adopt('cards', cards(revision, 40));
  backend.exit = [];
  backend.owner = (volume, muted) => agent.adopt('cards', cards(++revision, volume, muted));
  backend.harness = profileHarness({ snapshot: layoutProfilesFixtureSnapshot() });
  agent.adopt('layout', backend.harness.fixture.layout());
});
afterEach(() => { backend.harness?.stop(); cleanup(); agent.reset(); vi.useRealTimers(); });

it('sets the exit volume while dragging, coalesced, and lands the release value', async () => {
  const { container } = render(<App/>);
  const slider = container.querySelector<HTMLElement>('[role="slider"]')!;
  slider.getBoundingClientRect = () => ({ left: 0, top: 0, x: 0, y: 0, width: 100, height: 20, right: 100, bottom: 20, toJSON() {} });
  fireEvent.pointerDown(slider, { button: 0, clientX: 41, pointerId: 1 });
  for (let x = 42; x <= 90; x++) {
    fireEvent.pointerMove(slider, { clientX: x, pointerId: 1 });
    await act(async () => { await vi.advanceTimersByTimeAsync(8); });
  }
  // The slider follows the pointer, not the round trip.
  expect(slider.getAttribute('aria-valuenow')).toBe('90');
  fireEvent.pointerUp(slider, { pointerId: 1 });
  await act(async () => { await vi.advanceTimersByTimeAsync(200); });
  const sent = backend.exit;
  expect(sent.length).toBeGreaterThan(3);
  expect(sent.length).toBeLessThanOrEqual(Math.ceil(49 * 8 / 40) + 2);
  expect(sent.at(-1)).toMatchObject({ volumePercent: 90, muted: false });
  // Each send used the version the previous write produced, so none could be refused as stale.
  expect(new Set(sent.map(s => s.versionToken)).size).toBe(sent.length);
  expect(agent.get().cards?.exitVolume?.volumePercent).toBe(90);
  expect(slider.getAttribute('aria-valuenow')).toBe('90');
});

it('applies keyboard steps at once through the same coalescing', async () => {
  const { container } = render(<App/>);
  const slider = container.querySelector<HTMLElement>('[role="slider"]')!;
  for (let i = 0; i < 10; i++) fireEvent.keyDown(slider, { key: 'ArrowUp' });
  expect(slider.getAttribute('aria-valuenow')).toBe('50');
  await act(async () => { await vi.advanceTimersByTimeAsync(200); });
  expect(backend.exit.length).toBeLessThanOrEqual(3);
  expect(backend.exit.at(-1)).toMatchObject({ volumePercent: 50 });
  expect(agent.get().cards?.exitVolume?.volumePercent).toBe(50);
});
