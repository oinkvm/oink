import { useRef, useState } from 'react';
/** `meta` describes the transition between a step's value and the value after it,
 * so it travels with the step through undo and redo unchanged. */
export interface HistoryStep<T, M = never> {
    value: T;
    label: string;
    meta?: M;
}
export interface HistoryMove<T, M = never> {
    value: T;
    label: string;
    meta?: M;
    undoCount: number;
    redoCount: number;
}
export interface HistoryOptions<T, M = never> {
    limit?: number;
    mergeWindow?: number;
    equals?: (a: T, b: T) => boolean;
    /** Combine an entry's metadata with a merged later edit's. Default: keep the later one. */
    mergeMeta?: (earlier: M | undefined, later: M | undefined) => M | undefined;
}
interface HistoryState<T, M> {
    past: HistoryStep<T, M>[];
    present: T;
    future: HistoryStep<T, M>[];
}
export function useHistory<T, M = never>(initial: () => T, { limit = 100, mergeWindow = 600, equals = Object.is, mergeMeta = (_, later) => later }: HistoryOptions<T, M> = {}) {
    const [state, setState] = useState<HistoryState<T, M>>(() => ({ past: [], present: initial(), future: [] }));
    const latest = useRef(state);
    const lastMerge = useRef<{
        key: string;
        at: number;
    } | null>(null);
    const apply = (s: HistoryState<T, M>) => {
        latest.current = s;
        setState(s);
    };
    const commit = (next: T, label: string, merge?: string, meta?: M) => {
        const s = latest.current;
        if (equals(next, s.present))
            return;
        const now = performance.now();
        const m = lastMerge.current;
        const merging = !!merge && !!m && m.key === merge && now - m.at < mergeWindow && s.past.length > 0;
        lastMerge.current = merge ? { key: merge, at: now } : null;
        const last = s.past[s.past.length - 1];
        const past = merging ? [...s.past.slice(0, -1), { ...last, meta: mergeMeta(last.meta, meta) }] : [...s.past, { value: s.present, label, meta }].slice(-limit);
        apply({ past, present: next, future: [] });
    };
    const undo = (): HistoryMove<T, M> | null => {
        const s = latest.current;
        const step = s.past[s.past.length - 1];
        if (!step)
            return null;
        lastMerge.current = null;
        const past = s.past.slice(0, -1);
        const future = [{ value: s.present, label: step.label, meta: step.meta }, ...s.future];
        apply({ past, present: step.value, future });
        return { value: step.value, label: step.label, meta: step.meta, undoCount: past.length, redoCount: future.length };
    };
    const redo = (): HistoryMove<T, M> | null => {
        const s = latest.current;
        const step = s.future[0];
        if (!step)
            return null;
        lastMerge.current = null;
        const past = [...s.past, { value: s.present, label: step.label, meta: step.meta }];
        const future = s.future.slice(1);
        apply({ past, present: step.value, future });
        return { value: step.value, label: step.label, meta: step.meta, undoCount: past.length, redoCount: future.length };
    };
    return {
        value: state.present,
        checkpoint: () => latest.current,
        restore: (checkpoint: HistoryState<T, M>) => { lastMerge.current = null; apply(checkpoint); },
        sync: (next: T) => { if (!equals(next, latest.current.present))
            apply({ ...latest.current, present: next }); },
        /** Replace the newest entry's result with the authoritative one, keeping the
         * redo stack and the merge window: an optimistic edit became an observed one. */
        settle: (next: T, meta: (current: M | undefined) => M | undefined) => {
            const s = latest.current, last = s.past[s.past.length - 1];
            apply({ ...s, present: next, past: last ? [...s.past.slice(0, -1), { ...last, meta: meta(last.meta) }] : s.past });
        },
        commit,
        undo,
        redo,
        undoCount: state.past.length,
        redoCount: state.future.length,
    };
}
