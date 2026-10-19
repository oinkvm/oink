import { useEffect, useRef, useState } from 'react';
import { speed } from 'oink-ui';
export type ItemPhase = 'idle' | 'enter' | 'leave';
interface Entry<T> {
    item: T;
    phase: ItemPhase;
}
export function useListPresence<T extends {
    id: string;
}>(items: T[], exitMs = 420): Array<{
    item: T;
    phase: ItemPhase;
}> {
    const [state, setState] = useState<{
        source: T[];
        list: Entry<T>[];
    }>(() => ({
        source: items,
        list: items.map((item) => ({ item, phase: 'idle' })),
    }));
    let list = state.list;
    if (state.source !== items) {
        const ids = new Set(items.map((i) => i.id));
        const before = new Map(state.list.map((e) => [e.item.id, e]));
        const next: Entry<T>[] = items.map((item) => {
            const old = before.get(item.id);
            return { item, phase: !old ? 'enter' : old.phase === 'leave' ? 'idle' : old.phase };
        });
        state.list.forEach((e, index) => {
            if (ids.has(e.item.id))
                return;
            next.splice(Math.min(index, next.length), 0, { item: e.item, phase: 'leave' });
        });
        list = next;
        setState({ source: items, list: next });
    }
    const timers = useRef(new Map<string, number>());
    useEffect(() => {
        for (const e of list) {
            if (e.phase !== 'leave' || timers.current.has(e.item.id))
                continue;
            const id = e.item.id;
            const t = window.setTimeout(() => {
                timers.current.delete(id);
                setState((s) => ({ ...s, list: s.list.filter((x) => !(x.item.id === id && x.phase === 'leave')) }));
            }, exitMs * Math.max(speed(), 0.01));
            timers.current.set(id, t);
        }
        for (const [id, t] of timers.current) {
            if (!list.some((e) => e.item.id === id && e.phase === 'leave')) {
                window.clearTimeout(t);
                timers.current.delete(id);
            }
        }
    });
    useEffect(() => () => { timers.current.forEach((t) => window.clearTimeout(t)); }, []);
    return list;
}
