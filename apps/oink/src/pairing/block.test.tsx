// @vitest-environment jsdom
import { act, cleanup, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { toast } from 'oink-ui';
import { api } from '../api/client';
import { agent } from '../api/store';
import { pairingApi } from '../api/pairing';
import { setLanguage } from '../i18n';
import { usePairing } from './usePairing';
vi.mock('oink-ui', () => ({ speed: () => 1, toast: vi.fn() }));
beforeEach(() => { setLanguage('en'); vi.clearAllMocks(); });
afterEach(() => { cleanup(); agent.reset(); vi.restoreAllMocks(); });

it.each(['incoming-pair', 'peer-id'])('blocks by device identity with normal confirmation and undo (%s)', async id => {
  const snapshot = { stream: 'blocks', revision: '1', entries: [] };
  const block = vi.spyOn(api, 'block').mockResolvedValue({ operation_id: 'block', changed: true, snapshot });
  const unblock = vi.spyOn(api, 'unblock').mockResolvedValue({ operation_id: 'unblock', changed: true, snapshot: { ...snapshot, revision: '2' } });
  const cancel = vi.spyOn(pairingApi, 'cancel');
  const decline = vi.spyOn(pairingApi, 'decline');
  const { result } = renderHook(() => usePairing());
  await act(async () => result.current.actions.block({ id, device: 'peer-id', compatible: true,
    name: 'Peer', os: 'win', ip: '', port: 47470, deviceId: 'fingerprint' }));
  expect(block).toHaveBeenCalledExactlyOnceWith('peer-id');
  expect(cancel).not.toHaveBeenCalled();
  expect(decline).not.toHaveBeenCalled();
  expect(agent.get().blocked).toEqual(snapshot);
  expect(toast).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ title: 'Blocked Peer' }));
  const confirmation = vi.mocked(toast).mock.calls[0][0];
  if (typeof confirmation === 'string') throw new Error('Expected Block confirmation with Undo');
  await act(async () => { confirmation.action!.onClick!(); });
  expect(unblock).toHaveBeenCalledExactlyOnceWith('peer-id');
});
