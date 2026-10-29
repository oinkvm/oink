import { useCallback, useSyncExternalStore } from 'react';
import { agent } from '../api/store';
import type { Device } from './types';

export function useCardLatency(device: Device): number | null {
  const subscribe = useCallback((changed: () => void) => agent.cardSignals.subscribe(device.id, changed), [device.id]);
  const read = useCallback(() => agent.cardSignals.get(device.id), [device.id]);
  const latency = useSyncExternalStore(subscribe, read);
  return device.group === 'online' ? latency === undefined ? device.latency : latency : null;
}
