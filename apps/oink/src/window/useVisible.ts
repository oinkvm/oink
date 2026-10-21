import { useEffect, useState } from 'react';
import type { RefObject } from 'react';

/** Viewport/document notifications, with no polling or animation clock. */
export function useVisible(element: RefObject<HTMLElement | null>, active = true): boolean {
  const [visible, setVisible] = useState(() => !document.hidden && typeof IntersectionObserver === 'undefined');
  useEffect(() => {
    if (!active || !element.current) { setVisible(false); return; }
    let intersecting = typeof IntersectionObserver === 'undefined';
    const update = () => setVisible(intersecting && !document.hidden);
    const observer = typeof IntersectionObserver === 'undefined' ? undefined : new IntersectionObserver(([entry]) => {
      // With the default threshold (0), edge contact is already intersecting.
      // Requiring a positive ratio can latch false: entering further does not
      // cross another threshold, so the observer need not notify again.
      intersecting = entry.isIntersecting;
      update();
    });
    observer?.observe(element.current);
    document.addEventListener('visibilitychange', update);
    update();
    return () => { observer?.disconnect(); document.removeEventListener('visibilitychange', update); };
  }, [active, element]);
  return active && visible;
}
