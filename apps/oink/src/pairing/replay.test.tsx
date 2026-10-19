// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, renderHook, waitFor } from '@testing-library/react';
import { agent } from '../api/store';
import { pairingApi } from '../api/pairing';
import { usePairing } from './usePairing';
import type { PairingEvent } from '../api/pairing';
import type { Overview } from '../api/types';
beforeEach(() => agent.adopt('overview', { this_device: { id: 'local' } } as Overview));
vi.mock('oink-ui', () => ({ speed: () => 1, toast: vi.fn() }));
afterEach(() => { cleanup(); agent.reset(); localStorage.clear(); vi.restoreAllMocks(); });
describe('durable pairing replay', () => {
  it('consumes a returned replay event and persists its cursor before acknowledging', async () => {
    localStorage.setItem('oink:pairing-cursor:local', '9007199254740992');
    const event: PairingEvent = { type: 'pairing_failed', data: { event_id: '9007199254740993', pair_id: 'pair', peer: { address: { family: 0, address: [127, 0, 0, 1], port: 47470, interface_scope: 0 }, expected_id: null, verified: null }, problem: { code: 44, stage: 3, retry_after_ms: null, attempts_left: null, winner: null, needed: null, limit: null, protocol_code: null, limit_kind: null, cause: null } } };
    const subscribe = vi.spyOn(pairingApi, 'subscribe').mockResolvedValue([event]);
    const ack = vi.spyOn(pairingApi, 'ack').mockImplementation(async id => {
      expect(localStorage.getItem('oink:pairing-cursor:local')).toBe(id);
      return null;
    });
    act(() => agent.adopt('boot', { build: null, version: '0.1.0-beta.1', platform: 'macos', backend: 'agent' }));
    renderHook(() => usePairing(true));
    await waitFor(() => expect(ack).toHaveBeenCalledWith('9007199254740993'));
    expect(subscribe).toHaveBeenCalledWith('9007199254740992');
    await waitFor(() => expect(agent.get().pairingEvents).toEqual([]));
  });
  it('leaves durable replay to the main window', () => {
    const subscribe = vi.spyOn(pairingApi, 'subscribe').mockResolvedValue([]);
    agent.adopt('boot', { build: null, version: '0.1.0-beta.1', platform: 'macos', backend: 'agent' });
    renderHook(() => usePairing(false));
    expect(subscribe).not.toHaveBeenCalled();
  });
});
