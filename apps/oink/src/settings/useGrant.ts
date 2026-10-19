import { useRef, useState } from 'react';
import { agent } from '../api/store';
import type { PermissionId } from './prefs';

/** A first click requests permission; a later explicit click opens Settings. */
export function useGrant(onGrant: (id: PermissionId) => void | Promise<void>, onOpenSettings?: (id: PermissionId) => void | Promise<void>) {
  const [pending, setPending] = useState<Set<PermissionId>>(new Set());
  const [requested, setRequested] = useState<Set<PermissionId>>(new Set());
  const busy = useRef(new Set<PermissionId>());
  const grant = async (id: PermissionId) => {
    if (busy.current.has(id)) return;
    busy.current.add(id);
    setPending(new Set(busy.current));
    try {
      if (requested.has(id) && id !== 'files' && onOpenSettings) await onOpenSettings(id);
      else {
        await onGrant(id);
        setRequested(previous => new Set(previous).add(id));
      }
    } catch { agent.fail('permission_denied'); }
    finally {
      busy.current.delete(id);
      setPending(new Set(busy.current));
    }
  };
  return { pending, requested, grant, canOpenSettings: !!onOpenSettings };
}
