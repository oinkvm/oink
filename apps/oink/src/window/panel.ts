import { shell } from '../api/client';
import { agent } from '../api/store';

/** Keep the native panel hidden until its measured WebView has painted. */
export function observePanel(element: HTMLElement): () => void {
  let height = 0;
  let generation = 0;
  let stopped = false;
  let frame = 0;
  const observer = new ResizeObserver(() => {
    const next = Math.ceil(element.scrollHeight);
    if (next === height) return;
    height = next;
    const measurement = ++generation;
    cancelAnimationFrame(frame);
    void shell.resizePanel(360, next).then(() => {
      if (stopped || measurement !== generation) return;
      // ResizeObserver runs before paint. Give WebKit a paint at the native
      // size, including oink-ui's scripted superellipse corner clipping.
      frame = requestAnimationFrame(() => {
        frame = requestAnimationFrame(() => {
          if (!stopped && measurement === generation) {
            void shell.showPanel().catch(() => agent.fail('unavailable'));
          }
        });
      });
    }).catch(() => { if (!stopped) agent.fail('unavailable'); });
  });
  observer.observe(element);
  return () => {
    stopped = true;
    cancelAnimationFrame(frame);
    observer.disconnect();
  };
}
