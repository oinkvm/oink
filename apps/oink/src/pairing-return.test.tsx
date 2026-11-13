// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { App } from './App';
import { agent } from './api/store';
import { pairProblem, pairingApi } from './api/pairing';
import type { PairView, PendingRequest, PairingSnapshot, PeerView } from './api/pairing';
import { setLanguage } from './i18n';

vi.mock('./api/store', async original => ({ ...await original<typeof import('./api/store')>(), start: vi.fn() }));
vi.mock('./api/transport', async original => ({
  ...await original<typeof import('./api/transport')>(),
  transport: {
    call: async ({ method }: { method: string }) => method === 'group' ? { members: [] } : [],
    shell: async () => null,
    onEvent: async () => () => {},
  },
}));
beforeAll(() => {
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
  Element.prototype.scrollTo = () => {};
});

const device = 'b'.repeat(64);
const address = { family: 0, address: [192, 0, 2, 1], port: 47470, interface_scope: 0 };
const peer: PeerView = { device, name: 'Peer', os: 'macos', remote: address, listen: null };
const session = (direction: PairView['direction'], phase: number, revision = '1'): PairView =>
  ({ id: 'pair', revision, direction, phase, peer: { address, expected_id: device, verified: peer }, attempts_left: 3, feedback: 'waiting', terminal: phase >= 8 ? pairProblem(1) : null });
const request: PendingRequest = { id: 'pair', revision: '1', peer, source: { group: 'source', root: 'source' }, target: { group: 'target', root: 'target' }, source_count: 1, target_count: 1 };
const nearbyRow = { device, name: 'Peer', os: 'macos', group: null, count: 1, addresses: [], selected: address, source: 0, identity: 'advertised', reachable: true, compatible: true, problem: null };
const snapshot = (sessions: PairView[], requests: PendingRequest[] = []): PairingSnapshot => ({ revision: '1', requests, sessions });

const view = () => document.querySelector('.nav-view')?.getAttribute('data-view');
const dialog = () => document.querySelector<HTMLElement>('.pairdlg')!;
const finish = (direction: PairView['direction'], phase: number) =>
  act(() => agent.receive({ type: 'pairing_changed', data: { revision: '2', view: session(direction, phase, '2') } }));

// Starts a pairing from the add-device page: the nearby list (initiated) or the
// requests group (accepted). The pair dialog then shows its code step.
async function startPairing(how: 'initiated' | 'accepted') {
  const direction = how === 'initiated' ? 'outgoing' : 'incoming';
  vi.spyOn(pairingApi, 'start').mockResolvedValue({ type: 'started', pair_id: 'pair' });
  vi.spyOn(pairingApi, 'accept').mockResolvedValue({ type: 'accepted' });
  vi.spyOn(pairingApi, 'cancel').mockResolvedValue({ type: 'cancelled' });
  vi.spyOn(pairingApi, 'pairings').mockResolvedValue(snapshot([session(direction, 2)]));
  agent.adopt('pairings', snapshot([], how === 'accepted' ? [request] : []));
  agent.adopt('nearby', { revision: '1', rows: how === 'initiated' ? [nearbyRow] : [] } as never);
  render(<App/>);
  fireEvent.click(document.querySelector('[data-action="add"]')!);
  expect(view()).toBe('add');
  await act(async () => {
    fireEvent.click(screen.getByRole('button', { name: how === 'initiated' ? 'Connect to Peer' : 'Accept pairing with Peer' }));
  });
  await waitFor(() => expect(dialog()).not.toBeNull());
  expect(within(dialog()).getByText('Pair with Peer')).toBeDefined();
  return direction;
}
const closed = () => waitFor(() => expect(document.querySelector('.pairdlg')).toBeNull(), { timeout: 3000 });

for (const platform of ['macos', 'windows'] as const) {
  describe(`${platform}: the device list after pairing`, () => {
    beforeEach(() => {
      setLanguage('en');
      agent.adopt('boot', { platform, version: '0.1.0-beta.1', build: null, backend: 'agent' });
    });
    afterEach(() => { cleanup(); agent.reset(); localStorage.clear(); vi.restoreAllMocks(); });

    it.each(['initiated', 'accepted'] as const)('returns to the device list once a %s pairing succeeds and its dialog closes', async how => {
      const direction = await startPairing(how);
      finish(direction, 7);
      await waitFor(() => expect(within(dialog()).getByText('Paired successfully')).toBeDefined());
      expect(view()).toBe('add');
      await closed();
      // The card unmounts in the same commit as the exit callback; navigation renders next.
      await waitFor(() => expect(view()).toBe('devices'));
      expect(document.querySelector('[data-selected]')).toBeNull();
    });

    it.each(['initiated', 'accepted'] as const)('stays on the add-device page when a %s pairing fails', async how => {
      const direction = await startPairing(how);
      finish(direction, 8);
      await waitFor(() => expect(within(dialog()).getByText('Pairing failed')).toBeDefined());
      fireEvent.click(within(dialog()).getByRole('button', { name: 'Close' }));
      await closed();
      await act(async () => { await new Promise(resolve => setTimeout(resolve, 50)); });
      expect(view()).toBe('add');
    });

    it.each(['initiated', 'accepted'] as const)('stays on the add-device page when a %s pairing is cancelled', async how => {
      await startPairing(how);
      fireEvent.click(within(dialog()).getByRole('button', { name: 'Cancel' }));
      await waitFor(() => expect(pairingApi.cancel).toHaveBeenCalledOnce());
      await closed();
      await act(async () => { await new Promise(resolve => setTimeout(resolve, 50)); });
      expect(view()).toBe('add');
    });
  });
}
