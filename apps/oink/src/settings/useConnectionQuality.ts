import { useEffect, useState } from 'react';
import { api, onAgentEvent } from '../api/client';
import type { ConnectionQuality } from '../api/types';

/** Subscribe before querying so opening a page cannot miss the next source tick. */
export function useConnectionQuality(active: boolean) {
  const [snapshot, setSnapshot] = useState<ConnectionQuality | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    if (!active) return;
    let closed = false;
    let stop: (() => void) | undefined;
    const adopt = (next: ConnectionQuality) => {
      if (!closed) {
        setFailed(false);
        setSnapshot(previous => !previous || next.revision > previous.revision || (next.revision === previous.revision && next.sampled_at_ms > previous.sampled_at_ms) ? next : previous);
      }
    };
    void (async () => {
      stop = await onAgentEvent(event => { if (event.type === 'connection_quality') adopt(event.data); });
      if (closed) { stop(); return; }
      adopt(await api.connectionQuality());
    })().catch(() => { if (!closed) setFailed(true); });
    return () => { closed = true; stop?.(); };
  }, [active]);
  return { snapshot, failed };
}
