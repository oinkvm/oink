import { useEffect, useState } from 'react';
import type { RefObject } from 'react';
import { shell } from '../api/client';
import { inTauri } from '../api/transport';

/** Follow the native control centre, including the shell's transparent gutter. */
export function useTrafficLightAlignment(bar: RefObject<HTMLElement | null>): number {
  const [offset, setOffset] = useState(0);
  useEffect(() => {
    if (!inTauri || !bar.current) return;
    let disposed = false, frame = 0, revision = 0;
    const update = () => {
      const current = ++revision;
      cancelAnimationFrame(frame);
      // Measure after the WebView and its CSS gutter respond to native resize.
      frame = requestAnimationFrame(() => {
        void shell.titlebarCenter().then(center => {
          if (disposed || current !== revision || !bar.current) return;
          const rect = bar.current.getBoundingClientRect();
          setOffset(center === null ? 0 : center - rect.top - rect.height / 2);
        }).catch(() => { /* The window may be closing; keep its current alignment. */ });
      });
    };
    const observer = new ResizeObserver(update);
    observer.observe(bar.current);
    window.addEventListener('resize', update);
    window.addEventListener('focus', update);
    update();
    return () => {
      disposed = true;
      cancelAnimationFrame(frame);
      observer.disconnect();
      window.removeEventListener('resize', update);
      window.removeEventListener('focus', update);
    };
  }, [bar]);
  return offset;
}
