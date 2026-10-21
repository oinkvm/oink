// @vitest-environment jsdom
import { act, renderHook } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { useHistory } from './useHistory';

describe('arrangement command history', () => {
  it('retains redo across an authoritative echo and restores a refused undo', () => {
    const { result } = renderHook(() => useHistory(() => 1));
    act(() => result.current.commit(2, 'Move'));
    const checkpoint = result.current.checkpoint();
    act(() => { result.current.undo(); result.current.sync(1); });
    expect(result.current.redoCount).toBe(1);
    act(() => result.current.restore(checkpoint));
    expect(result.current.value).toBe(2);
    expect(result.current.undoCount).toBe(1);
    expect(result.current.redoCount).toBe(0);
  });
  it('keeps operation endpoints on a step through undo, redo, merges and settling', () => {
    type Meta = { before: number; after: number };
    const { result } = renderHook(() => useHistory<number, Meta>(() => 0, { mergeMeta: (a, b) => a && b ? { before: a.before, after: b.after } : b }));
    act(() => result.current.commit(1, 'Nudge', 'key', { before: 0, after: 1 }));
    act(() => result.current.commit(2, 'Nudge', 'key', { before: 1, after: 2 }));
    act(() => result.current.settle(3, m => m && { ...m, after: 3 }));
    expect(result.current.undoCount).toBe(1);
    let move: ReturnType<typeof result.current.undo> = null;
    act(() => { move = result.current.undo(); });
    expect(move!.meta).toEqual({ before: 0, after: 3 });
    act(() => { result.current.sync(-1); });
    act(() => { move = result.current.redo(); });
    expect(move!.meta).toEqual({ before: 0, after: 3 });
    expect(result.current.value).toBe(3);
  });
  it('makes a successful arrangement reset undoable', () => {
    const { result } = renderHook(() => useHistory(() => 3));
    act(() => result.current.commit(0, 'Reset'));
    act(() => { result.current.undo(); });
    expect(result.current.value).toBe(3);
  });
});
