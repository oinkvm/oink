import { beforeEach, describe, expect, it, vi } from 'vitest';
const { nativeCall } = vi.hoisted(() => ({ nativeCall: vi.fn() }));
vi.mock('./transport', async importOriginal => ({ ...await importOriginal<typeof import('./transport')>(), transport: { call: nativeCall } }));
import { api } from './client';
import { createAgentStore } from './store';
import type { DeviceControlReply } from './cards';
beforeEach(() => nativeCall.mockReset());
describe('final native control receipts', () => {
  it.each([
    () => api.invokeCard('request', 'token'),
    () => api.setForwardingControls('request', 'version', 100, false),
    () => api.setExitVolume('request', 'version', 100, false),
    () => api.setLocalPaused('request', true),
    () => api.setDevicePaused('request', 'token', true),
  ])('preserves immediate refusal envelopes for optimistic rollback', async command => {
    const store = createAgentStore();
    store.pending({ requestId: 'request', device: 'owner', resource: 'mouse', desired: 'local' });
    const receipt: DeviceControlReply = { type: 'device_control_completed', data: { requestId: 'request', result: { outcome: 'refused', reason: 'stale_revision', unknownCode: null } } };
    nativeCall.mockResolvedValue(receipt);
    const reply = await command();
    store.controlReply(reply);
    expect(store.get().pending).toEqual([]);
    expect(store.get().error).toBe('stale_revision');
    expect(store.get().cards).toBeNull();
  });
});
