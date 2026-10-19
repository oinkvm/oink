import { useRef, useState } from 'react';
export interface HistoryStep<T> {
    value: T;
    label: string;
}
export interface HistoryMove<T> {
    value: T;
    label: string;
    undoCount: number;
    redoCount: number;
}
export interface HistoryOptions<T> {
    limit?: number;
    mergeWindow?: number;
    equals?: (a: T, b: T) => boolean;
}
interface HistoryState<T> {
    past: HistoryStep<T>[];
    present: T;
    future: HistoryStep<T>[];
}
export function useHistory<T>(initial: () => T, { limit = 100, mergeWindow = 600, equals = Object.is }: HistoryOptions<T> = {}) {
    const [state, setState] = useState<HistoryState<T>>(() => ({ past: [], present: initial(), future: [] }));
    const latest = useRef(state);
    const lastMerge = useRef<{
        key: string;
        at: number;
    } | null>(null);
    const apply = (s: HistoryState<T>) => {
        latest.current = s;
        setState(s);
    };
    const commit = (next: T, label: string, merge?: string) => {
        const s = latest.current;
        if (equals(next, s.present))
            return;
        const now = performance.now();
        const m = lastMerge.current;
        const merging = !!merge && !!m && m.key === merge && now - m.at < mergeWindow && s.past.length > 0;
        lastMerge.current = merge ? { key: merge, at: now } : null;
        const past = merging ? s.past : [...s.past, { value: s.present, label }].slice(-limit);
        apply({ past, present: next, future: [] });
    };
    const undo = (): HistoryMove<T> | null => {
        const s = latest.current;
        const step = s.past[s.past.length - 1];
        if (!step)
            return null;
        lastMerge.current = null;
        const past = s.past.slice(0, -1);
        const future = [{ value: s.present, label: step.label }, ...s.future];
        apply({ past, present: step.value, future });
        return { value: step.value, label: step.label, undoCount: past.length, redoCount: future.length };
    };
    const redo = (): HistoryMove<T> | null => {
        const s = latest.current;
        const step = s.future[0];
        if (!step)
            return null;
        lastMerge.current = null;
        const past = [...s.past, { value: s.present, label: step.label }];
        const future = s.future.slice(1);
        apply({ past, present: step.value, future });
        return { value: step.value, label: step.label, undoCount: past.length, redoCount: future.length };
    };
    return {
        value: state.present,
        checkpoint: () => latest.current,
        restore: (checkpoint: HistoryState<T>) => { lastMerge.current = null; apply(checkpoint); },
        sync: (next: T) => { if (!equals(next, latest.current.present))
            apply({ ...latest.current, present: next }); },
        commit,
        undo,
        redo,
        undoCount: state.past.length,
        redoCount: state.future.length,
    };
}
