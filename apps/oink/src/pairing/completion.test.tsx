// @vitest-environment jsdom
import { act, cleanup, renderHook, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { agent } from '../api/store';
import { pairingApi } from '../api/pairing';
import type { PairView, PairingEvent, PeerView } from '../api/pairing';
import type { Overview } from '../api/types';
import { usePairing } from './usePairing';
import { toast } from 'oink-ui';
vi.mock('oink-ui', () => ({ speed: () => 1, toast: vi.fn() }));
const peer: PeerView = { device: 'remote', name: 'Peer', os: 'macos', remote: { family: 0, address: [192, 0, 2, 1], port: 47470, interface_scope: 0 }, listen: null };
const view = (phase = 2): PairView => ({ id: 'pair', revision: '1', direction: 'incoming', phase, peer: { address: peer.remote, expected_id: peer.device, verified: peer }, attempts_left: 3, feedback: 'waiting', terminal: null });
const complete: PairingEvent = { type: 'pairing_completed', data: { event_id: '1', pair_id: 'pair', peer, merge_id: 'merge', target_group: { group: 'group', root: 'root' }, imported_devices: [], undo: { group: { group: 'group', root: 'root' }, merge_id: 'merge', peer: 'remote', observed_add: 'add' }, reconciled: false } };
const overview = (id: string): Overview => ({ this_device: { id, fingerprint: id, name: 'Local', os: 'macos', os_version: '', model: null, app_version: '0.1.0-beta.1' }, group: null, service: 'running', sharing_enabled: true, pause: { paused: false, scope: 'local', revision: 0, author: null, author_name: null, emergency: false, reason: "none", capture_detached: false, settling: false }, focus_lock: null, focus: null, onboarding_done: true, alerts: [] });
beforeEach(() => {
  agent.adopt('boot', { build: null, version: '0.1.0-beta.1', platform: 'macos', backend: 'agent' });
  agent.adopt('overview', overview('new-identity'));
  agent.adopt('pairings', { revision: '1', requests: [], sessions: [view()] });
  vi.spyOn(pairingApi, 'subscribe').mockResolvedValue([]);
  vi.spyOn(pairingApi, 'ack').mockResolvedValue(null);
  vi.spyOn(pairingApi, 'submit').mockResolvedValue({ type: 'accepted' });
});
afterEach(() => { cleanup(); agent.reset(); localStorage.clear(); vi.restoreAllMocks(); vi.clearAllMocks(); });
describe('pairing completion', () => {
  it('keeps a dismissed accepted request resumable without accepting or cancelling it again', async () => {
    const accept = vi.spyOn(pairingApi, 'accept').mockResolvedValue({ type: 'accepted' });
    const cancel = vi.spyOn(pairingApi, 'cancel').mockResolvedValue({ type: 'cancelled' });
    vi.spyOn(pairingApi, 'pairings').mockImplementation(async () => agent.get().pairings!);
    const { result } = renderHook(() => usePairing());
    expect(result.current.dialog.open).toBe(true);
    act(() => result.current.actions.dismiss());
    expect(result.current.dialog.open).toBe(false);
    expect(result.current.requests.map(r => r.id)).toEqual(['pair']);
    act(() => agent.receive({ type: 'pairing_changed', data: { revision: '2', view: { ...view(), revision: '2', feedback: 'wrong' } } }));
    expect(result.current.dialog.open).toBe(false);
    await act(async () => result.current.actions.accept(result.current.requests[0]));
    expect(result.current.dialog.open).toBe(true);
    expect(result.current.dialog.session?.status).toBe('wrong');
    expect(accept).not.toHaveBeenCalled(); expect(cancel).not.toHaveBeenCalled();
  });
  it('handles reused event IDs after a fresh device reset instead of silently acknowledging success', async () => {
    localStorage.setItem('oink:pairing-cursor', '9');
    localStorage.setItem('oink:pairing-cursor:old-identity', '9');
    const { result } = renderHook(() => usePairing());
    await act(async () => { agent.receive(complete); });
    expect(result.current.dialog.session?.status).toBe('success');
    expect(toast).toHaveBeenCalledOnce();
    expect(pairingApi.subscribe).toHaveBeenCalledWith('0');
    expect(localStorage.getItem('oink:pairing-cursor:new-identity')).toBe('1');
  });
  it('lets completion win while the last code submission is still in flight', async () => {
    let resolve!: (value: { type: 'accepted' }) => void;
    vi.mocked(pairingApi.submit).mockImplementation(() => new Promise(r => { resolve = r; }));
    const { result } = renderHook(() => usePairing());
    let pending!: Promise<void>;
    act(() => { pending = result.current.actions.submit('123456'); });
    expect(result.current.dialog.session?.status).toBe('verifying');
    act(() => agent.receive(complete));
    expect(result.current.dialog.session?.status).toBe('success');
    await act(async () => { resolve({ type: 'accepted' }); await pending; });
  });
  it.each(['incoming', 'outgoing'] as const)('uses authoritative Completed phase if the %s receipt was already acknowledged', async direction => {
    const { result } = renderHook(() => usePairing());
    act(() => agent.receive({ type: 'pairing_changed', data: { revision: '2', view: { ...view(7), direction, revision: '2', feedback: 'correct' } } }));
    expect(result.current.dialog.session?.status).toBe('success');
    await waitFor(() => expect(result.current.dialog.open).toBe(false));
  });
  it('waits for local identity before replaying or acknowledging an event', async () => {
    agent.adopt('overview', null);
    renderHook(() => usePairing());
    act(() => agent.receive(complete));
    expect(pairingApi.subscribe).not.toHaveBeenCalled();
    expect(pairingApi.ack).not.toHaveBeenCalled();
    act(() => agent.adopt('overview', overview('new-identity')));
    await waitFor(() => expect(pairingApi.ack).toHaveBeenCalledWith('1'));
    expect(toast).toHaveBeenCalledOnce();
  });
});
