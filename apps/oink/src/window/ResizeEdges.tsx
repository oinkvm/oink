import { inTauri } from '../api/transport';

const directions = ['North', 'NorthEast', 'East', 'SouthEast', 'South', 'SouthWest', 'West', 'NorthWest'] as const;

/** The visible shell sits inside the native window's transparent shadow gutter. */
export function ResizeEdges({ platform }: { platform: 'mac' | 'win' }) {
  if (!inTauri) return null;
  return <div className="resize-edges" aria-hidden>
    {directions.map(direction => <div key={direction} data-direction={direction} onPointerDown={event => {
      // AppKit tracks Mac edge gestures before they reach the WebView because
      // Tao's startResizeDragging is unsupported there.
      if (event.button !== 0 || platform === 'mac') return;
      event.preventDefault();
      void import('@tauri-apps/api/window').then(({ getCurrentWindow }) => getCurrentWindow().startResizeDragging(direction));
    }}/>) }
  </div>;
}
