import type { MemberRoutes } from './types';

/** Route submenu data (route-selection design §6.5). The backend measures in
 * the background once a minute; the menu reads the latest result when it
 * opens and nothing is pushed while it stays open. Values never touch the
 * global agent snapshot. */
export function createRoutesStore() {
  let values = new Map<string, MemberRoutes>();
  const listeners = new Map<string, Set<() => void>>();
  const notify = (device: string) => listeners.get(device)?.forEach(callback => callback());
  return {
    get: (device: string) => values.get(device),
    subscribe(device: string, callback: () => void) {
      let callbacks = listeners.get(device);
      if (!callbacks) listeners.set(device, callbacks = new Set());
      callbacks.add(callback);
      return () => { callbacks.delete(callback); if (!callbacks.size) listeners.delete(device); };
    },
    /** One device's reply (a menu opening, or a read after a choice). */
    adopt(member: MemberRoutes) {
      values = new Map(values).set(member.device, member);
      notify(member.device);
    },
  };
}
