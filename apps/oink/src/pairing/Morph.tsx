import { useLayoutEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { speed } from 'oink-ui';

type View = { key: string; children: ReactNode };

/** Keep the last usable view while its replacement is connecting. */
export function Morph({ viewKey, pending = false, immediate = false, children }: {
    viewKey: string;
    pending?: boolean;
    /** Terminal errors replace stale content without an outgoing overlay. */
    immediate?: boolean;
    children: ReactNode;
}) {
    const [view, setView] = useState<View & { previous?: View }>({ key: viewKey, children });
    const latest = useRef<View>(view);
    const el = useRef<HTMLDivElement>(null);
    const incoming = useRef<HTMLDivElement>(null);
    const outgoing = useRef<HTMLDivElement>(null);
    const height = useRef(0);
    const waiting = view.key !== viewKey;
    if ((waiting && !pending) || (immediate && view.previous)) {
        setView({ key: viewKey, children, previous: immediate ? undefined : latest.current });
    }
    useLayoutEffect(() => {
        if (!waiting) latest.current = { key: viewKey, children };
    });
    useLayoutEffect(() => {
        const node = el.current!;
        const next = incoming.current!;
        const old = outgoing.current;
        const from = height.current;
        // Computed height retains subpixels without including the dialog entrance scale.
        const to = parseFloat(getComputedStyle(next).height);
        height.current = to;
        if (!old) return;
        const finish = () => setView(v => v.key === view.key ? { key: v.key, children: v.children } : v);
        const duration = 360 * speed();
        if (!from || !to || duration <= 0 || typeof node.animate !== 'function') {
            finish();
            return;
        }
        // A taller layer only fades in once it fits; a taller outgoing layer
        // fades out before shrinking. Neither layer needs an overflow mask.
        let finished = false;
        const growing = to > from;
        const shrinking = to < from;
        const size = node.animate(growing ? [
            { height: `${from}px`, easing: 'cubic-bezier(.22, 1, .36, 1)' }, { height: `${to}px`, offset: .7 }, { height: `${to}px` },
        ] : [
            { height: `${from}px` }, { height: `${from}px`, offset: .3, easing: 'cubic-bezier(.22, 1, .36, 1)' }, { height: `${to}px` },
        ], { duration });
        const fadeIn = next.animate(growing ? [
            { opacity: 0 }, { opacity: 0, offset: .7 }, { opacity: 1 },
        ] : [{ opacity: 0 }, { opacity: 1 }], { duration, fill: 'both' });
        const fadeOut = old.animate(shrinking ? [
            { opacity: 1 }, { opacity: 0, offset: .3 }, { opacity: 0 },
        ] : [{ opacity: 1 }, { opacity: 0 }], { duration, fill: 'both' });
        size.onfinish = () => { finished = true; size.cancel(); fadeIn.cancel(); fadeOut.cancel(); finish(); };
        return () => {
            // Read the displayed height before cancelling an interrupted morph.
            if (!finished) height.current = node.getBoundingClientRect().height;
            size.cancel(); fadeIn.cancel(); fadeOut.cancel();
        };
    }, [view.key]);
    useLayoutEffect(() => {
        if (view.previous || waiting) return;
        const observer = new ResizeObserver(() => { height.current = parseFloat(getComputedStyle(incoming.current!).height); });
        observer.observe(incoming.current!);
        return () => observer.disconnect();
    }, [view.key, view.previous, waiting]);
    return <div className="morph" ref={el}>
        {view.previous && <div key={view.previous.key} className="morph__out" ref={outgoing} inert aria-hidden="true">{view.previous.children}</div>}
        <div key={view.key} className="morph__in" ref={incoming} inert={waiting || undefined}>
            {waiting ? view.children : children}
        </div>
    </div>;
}
