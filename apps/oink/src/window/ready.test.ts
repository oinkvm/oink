// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { revealWhenReady } from './ready';
let frames: Map<number, FrameRequestCallback>;
let sequence: number;
let fontsReady: () => void;
const paint = () => {
  const pending = [...frames.values()]; frames.clear();
  pending.forEach(callback => callback(0));
};
beforeEach(() => {
  frames = new Map(); sequence = 0;
  Object.defineProperty(document, 'fonts', { configurable: true, value: { ready: new Promise<void>(r => { fontsReady = r; }) } });
  vi.stubGlobal('requestAnimationFrame', vi.fn((callback: FrameRequestCallback) => { frames.set(++sequence, callback); return sequence; }));
  vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id));
});
afterEach(() => vi.unstubAllGlobals());
it('reveals after DOM and fonts are ready even when an invisible WebView never produces frames', async () => {
  const reveal = vi.fn(); const stop = revealWhenReady(reveal);
  paint(); paint(); expect(reveal).not.toHaveBeenCalled();
  fontsReady(); await Promise.resolve();
  expect(reveal).toHaveBeenCalledOnce();
  expect(requestAnimationFrame).not.toHaveBeenCalled();
  paint(); paint(); expect(reveal).toHaveBeenCalledOnce(); stop();
});
it.each(['fonts', 'queued font completion'])('cancels a view closed during %s', async phase => {
  const reveal = vi.fn(); const stop = revealWhenReady(reveal);
  if (phase === 'queued font completion') fontsReady();
  stop(); fontsReady(); await Promise.resolve(); paint(); paint();
  expect(reveal).not.toHaveBeenCalled();
});
