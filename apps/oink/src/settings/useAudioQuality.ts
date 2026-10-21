import { useEffect, useState } from 'react';
import { api, onAgentEvent } from '../api/client';
import type { AudioQuality } from '../api/types';

/** Subscribe before querying so opening a page cannot miss the next source tick. */
export function useAudioQuality(active: boolean) {
  const [snapshot, setSnapshot] = useState<AudioQuality | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    if (!active) return;
    let closed = false;
    let stop: (() => void) | undefined;
    const adopt = (next: AudioQuality) => {
      if (!closed) {
        setFailed(false);
        setSnapshot(previous => !previous || next.revision > previous.revision || (next.revision === previous.revision && next.sampled_at_ms > previous.sampled_at_ms) ? next : previous);
      }
    };
    void (async () => {
      stop = await onAgentEvent(event => { if (event.type === 'audio_quality') adopt(event.data); });
      if (closed) { stop(); return; }
      adopt(await api.audioQuality());
    })().catch(() => { if (!closed) setFailed(true); });
    return () => { closed = true; stop?.(); };
  }, [active]);
  return { snapshot, failed };
}
