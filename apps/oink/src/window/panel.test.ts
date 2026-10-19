import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { observePanel } from './panel';
import { shell } from '../api/client';

vi.mock('../api/client', () => ({ shell: { resizePanel: vi.fn(), showPanel: vi.fn() } }));
vi.mock('../api/store', () => ({ agent: { fail: vi.fn() } }));

let resized: () => void;
let frames: Map<number, FrameRequestCallback>;
let sequence: number;
let disconnect: ReturnType<typeof vi.fn>;
const paint = () => {
  const pending = [...frames.values()];
  frames.clear();
  pending.forEach(callback => callback(0));
};
beforeEach(() => {
  vi.clearAllMocks();
  frames = new Map();
  sequence = 0;
  disconnect = vi.fn();
  vi.stubGlobal('ResizeObserver', class {
    constructor(callback: () => void) { resized = callback; }
    observe() {}
    disconnect = disconnect;
  });
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
    frames.set(++sequence, callback);
    return sequence;
  });
  vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id));
  vi.mocked(shell.resizePanel).mockResolvedValue();
  vi.mocked(shell.showPanel).mockResolvedValue();
});
afterEach(() => vi.unstubAllGlobals());

it('keeps the first frame hidden until native sizing completes and the WebView paints', async () => {
  let sized!: () => void;
  vi.mocked(shell.resizePanel).mockReturnValue(new Promise<void>(resolve => { sized = resolve; }));
  const stop = observePanel({ scrollHeight: 388.3 } as HTMLElement);
  resized();
  expect(shell.resizePanel).toHaveBeenCalledWith(360, 389);
  paint(); paint();
  expect(shell.showPanel).not.toHaveBeenCalled();
  sized(); await Promise.resolve();
  paint();
  expect(shell.showPanel).not.toHaveBeenCalled();
  paint();
  expect(shell.showPanel).toHaveBeenCalledTimes(1);
  stop();
});

it('replaces a pending measurement and does not reveal a destroyed view', async () => {
  const element = { scrollHeight: 389 } as HTMLElement;
  const stop = observePanel(element);
  resized(); await Promise.resolve();
  paint();
  Object.defineProperty(element, 'scrollHeight', { value: 430 });
  resized(); await Promise.resolve();
  paint();
  expect(shell.showPanel).not.toHaveBeenCalled();
  stop();
  paint();
  expect(shell.showPanel).not.toHaveBeenCalled();
  expect(disconnect).toHaveBeenCalledTimes(1);
});

it('ignores a late native sizing acknowledgement after cleanup', async () => {
  let sized!: () => void;
  vi.mocked(shell.resizePanel).mockReturnValue(new Promise<void>(resolve => { sized = resolve; }));
  const stop = observePanel({ scrollHeight: 389 } as HTMLElement);
  resized();
  stop();
  sized(); await Promise.resolve();
  paint(); paint();
  expect(shell.showPanel).not.toHaveBeenCalled();
});
