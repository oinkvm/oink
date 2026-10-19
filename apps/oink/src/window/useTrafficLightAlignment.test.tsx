// @vitest-environment jsdom
import { act, cleanup, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { shell } from '../api/client';
import { useTrafficLightAlignment } from './useTrafficLightAlignment';

vi.mock('../api/transport', () => ({ inTauri: true }));
vi.mock('../api/client', () => ({ shell: { titlebarCenter: vi.fn() } }));
let frames: Map<number, FrameRequestCallback>, next: number;
let bounds: { top: number; height: number };
beforeEach(() => {
  vi.clearAllMocks(); frames = new Map(); next = 0; bounds = {top:24,height:44};
  vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => { frames.set(++next,callback); return next; });
  vi.stubGlobal('cancelAnimationFrame', (id: number) => { frames.delete(id); });
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
const flush = () => act(async () => {
  const pending = [...frames.values()]; frames.clear();
  pending.forEach(callback => callback(0));
});
function mount() {
  const element = document.createElement('div');
  vi.spyOn(element,'getBoundingClientRect').mockImplementation(() => bounds as DOMRect);
  const bar = {current:element};
  return renderHook(() => useTrafficLightAlignment(bar));
}
describe('native traffic-light alignment', () => {
  it.each([{system:'macOS 14',center:50,offset:4},{system:'macOS 26',center:46,offset:0}])('uses the measured $system frame with its gutter',async ({center,offset}) => {
    vi.mocked(shell.titlebarCenter).mockResolvedValue(center);
    const {result} = mount(); await flush();
    expect(result.current).toBe(offset);
  });
  it('remeasures after resize/focus and resets when controls move to fullscreen chrome',async () => {
    vi.mocked(shell.titlebarCenter).mockResolvedValue(50);
    const {result} = mount(); await flush(); expect(result.current).toBe(4);
    bounds = {top:0,height:44};
    vi.mocked(shell.titlebarCenter).mockResolvedValue(null);
    act(() => window.dispatchEvent(new Event('resize'))); await flush();
    expect(result.current).toBe(0);
    bounds = {top:24,height:44};
    vi.mocked(shell.titlebarCenter).mockResolvedValue(50);
    act(() => window.dispatchEvent(new Event('focus'))); await flush();
    expect(result.current).toBe(4);
  });
  it('ignores a stale native response after a newer layout measurement',async () => {
    let finish!: (center:number) => void;
    vi.mocked(shell.titlebarCenter).mockReturnValueOnce(new Promise(resolve => {finish=resolve;})).mockResolvedValue(50);
    const {result,unmount} = mount(); await flush();
    act(() => window.dispatchEvent(new Event('resize'))); await flush();
    expect(result.current).toBe(4);
    await act(async () => finish(46)); expect(result.current).toBe(4);
    unmount();
    act(() => window.dispatchEvent(new Event('focus'))); await flush();
    expect(shell.titlebarCenter).toHaveBeenCalledTimes(2);
  });
});
