// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { revealWhenPainted } from './firstPaint';
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
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => { frames.set(++sequence, callback); return sequence; });
  vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id));
});
afterEach(() => vi.unstubAllGlobals());
it('waits for fonts and a full layout/paint before revealing the native window', async () => {
  const reveal = vi.fn(); const stop = revealWhenPainted(reveal);
  paint(); paint(); expect(reveal).not.toHaveBeenCalled();
  fontsReady(); await Promise.resolve();
  paint(); expect(reveal).not.toHaveBeenCalled();
  paint(); expect(reveal).toHaveBeenCalledOnce(); stop();
});
it.each(['fonts', 'layout'])('cancels a view closed during %s', async phase => {
  const reveal = vi.fn(); const stop = revealWhenPainted(reveal);
  if (phase === 'layout') { fontsReady(); await Promise.resolve(); paint(); }
  stop(); fontsReady(); await Promise.resolve(); paint(); paint();
  expect(reveal).not.toHaveBeenCalled();
});
