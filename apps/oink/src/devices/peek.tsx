import { createContext, useContext, useEffect, useId, useLayoutEffect, useRef, useState } from 'react';
import type { CSSProperties, FocusEvent, PointerEvent as ReactPointerEvent, ReactNode, RefObject } from 'react';
import { Portal, speed, usePresence } from 'oink-ui';
import { useCornerShape } from '../arrange/cornerShape';
type Peek = ReturnType<typeof usePeek>;
const PeekContext = createContext<Peek | null>(null);
export function usePeekScope() {
    return useContext(PeekContext);
}
export function PeekScope({ collapsed, suspended = false, render, onTarget, children }: {
    collapsed: boolean;
    suspended?: boolean;
    render: (id: string, close: () => void) => ReactNode;
    onTarget?: (id: string | null) => void;
    children: ReactNode;
}) {
    const peek = usePeek(collapsed, suspended);
    const shownId = peek.open && peek.target ? peek.target.id : null;
    useEffect(() => { onTarget?.(shownId); }, [shownId]);
    const shape = useCornerShape();
    const content = peek.presence.mounted && peek.target ? render(peek.target.id, peek.close) : null;
    return (<PeekContext.Provider value={peek}>
      {children}
      {content && peek.target && (<Portal>
          <PeekPanel presenceRef={peek.presence.ref as RefObject<HTMLDivElement>} state={peek.presence.state} rect={peek.target.rect} edge={peek.target.edge} n={shape.n} rk={shape.rk} mergeTop={peek.target.mergeTop} focus={peek.target.focus} onPointerEnter={peek.keep} onPointerLeave={peek.leave}>
            {content}
          </PeekPanel>
        </Portal>)}
    </PeekContext.Provider>);
}
const PEEK_ENTER = 112;
const PEEK_LEAVE = 120;
function usePeek(collapsed: boolean, suspended = false) {
    const [target, setTarget] = useState<{
        id: string;
        rect: DOMRect;
        edge: number;
        mergeTop: boolean;
        focus: boolean;
    } | null>(null);
    const focusEl = useRef<HTMLElement | null>(null);
    const targetEl = useRef<HTMLElement | null>(null);
    const releaseFocusEl = () => { focusEl.current?.removeAttribute('data-peek-focus'); focusEl.current = null; };
    const [open, setOpen] = useState(false);
    const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
    const presence = usePresence(open, 400);
    const clear = () => { clearTimeout(timer.current); timer.current = undefined; };
    const close = () => { clear(); setOpen(false); };
    const enter = (id: string, el: HTMLElement, focus = false) => {
        if (suspended)
            return;
        clear();
        const go = () => {
            const canvas = el.closest('.win')?.querySelector('.disp__canvas');
            const side = el.closest('.win__side');
            const edge = (canvas ?? side ?? el).getBoundingClientRect()[canvas ? 'left' : 'right'];
            const rect = el.getBoundingClientRect();
            const mergeTop = !!canvas && Math.abs(rect.top - canvas.getBoundingClientRect().top) < 1;
            targetEl.current = el;
            releaseFocusEl();
            if (focus) {
                focusEl.current = el;
                el.setAttribute('data-peek-focus', '');
            }
            setTarget({ id, rect, edge, mergeTop, focus });
            setOpen(true);
        };
        if (open)
            go();
        else
            timer.current = setTimeout(go, PEEK_ENTER * speed());
    };
    const leave = () => { clear(); timer.current = setTimeout(() => setOpen(false), PEEK_LEAVE * speed()); };
    useEffect(() => { if (!collapsed)
        close(); }, [collapsed]);
    useEffect(() => { if (suspended)
        close(); }, [suspended]);
    useEffect(() => {
        if (!open)
            return;
        const onScroll = () => close();
        const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape')
            close(); };
        window.addEventListener('scroll', onScroll, { capture: true, passive: true });
        document.addEventListener('keydown', onKey);
        return () => { window.removeEventListener('scroll', onScroll, { capture: true }); document.removeEventListener('keydown', onKey); };
    }, [open]);
    useEffect(() => () => { clear(); releaseFocusEl(); }, []);
    useEffect(() => {
        if (!open || !targetEl.current)
            return;
        const el = targetEl.current;
        const measure = () => {
            const r = el.getBoundingClientRect();
            setTarget(t => t && Math.abs(t.rect.top - r.top) > 0.5 ? { ...t, rect: r } : t);
        };
        // Sidebar changes and native resize can move the anchor. Observe those
        // changes instead of reading layout on every display frame while idle.
        const resize = new ResizeObserver(measure);
        for (let node: HTMLElement | null = el; node; node = node.parentElement) {
            resize.observe(node);
            if (node.classList.contains('win')) break;
        }
        const mutations = new MutationObserver(measure);
        mutations.observe(el.closest('.win__side') ?? el.parentElement ?? el, {
            childList: true, subtree: true, attributes: true, attributeFilter: ['class', 'style'],
        });
        const visibility = () => { if (document.hidden) close(); };
        const intersection = typeof IntersectionObserver === 'undefined' ? undefined : new IntersectionObserver(([entry]) => {
            if (!entry.isIntersecting || entry.intersectionRatio === 0) close();
        });
        intersection?.observe(el);
        document.addEventListener('visibilitychange', visibility);
        window.addEventListener('resize', measure);
        visibility();
        measure();
        return () => {
            resize.disconnect(); mutations.disconnect(); intersection?.disconnect();
            document.removeEventListener('visibilitychange', visibility);
            window.removeEventListener('resize', measure);
        };
    }, [open, target?.id]);
    useEffect(() => { if (!open)
        releaseFocusEl(); }, [open]);
    const handlers = (id: string): PeekHandlers => ({
        onPointerEnter: (e) => { if (e.pointerType !== 'touch')
            enter(id, e.currentTarget); },
        onPointerLeave: leave,
        onFocus: (e) => { if (e.currentTarget.matches(':focus-visible'))
            enter(id, e.currentTarget, true); },
        onBlur: leave,
    });
    return { target, open, presence, handlers, keep: clear, leave, close };
}
const PEEK_W = 272;
const FILLET = { x: 4, y: 4 };
function quarter(cx: number, cy: number, rx: number, ry: number, a: [
    number,
    number
], b: [
    number,
    number
], n: number, steps = 20) {
    const e = 2 / n;
    let d = '';
    for (let i = 1; i <= steps; i++) {
        const t = (i / steps) * (Math.PI / 2);
        const u = Math.cos(t) ** e, v = Math.sin(t) ** e;
        d += `L${(cx + (a[0] * u + b[0] * v) * rx).toFixed(2)} ${(cy + (a[1] * u + b[1] * v) * ry).toFixed(2)}`;
    }
    return d;
}
function peekShape(W: number, H: number, xs: number, R: number, fx: number, fy: number, n: number, mergeTop: boolean) {
    const lower = `L${W} ${H - R}` + quarter(W - R, H - R, R, R, [1, 0], [0, 1], n)
        + `L${xs + fx} ${H}` + quarter(xs + fx, H + fy, fx, fy, [0, -1], [-1, 0], n);
    const tail = `L${xs} ${H}L${R} ${H}` + quarter(R, H - R, R, R, [0, 1], [-1, 0], n);
    if (mergeTop) {
        const outline = quarter(W + fx, fy, fx, fy, [0, -1], [-1, 0], n) + lower;
        return {
            line: `M${W + fx} 0` + outline,
            fill: `M0 0L${W + fx} 0` + outline + tail + 'Z',
            shadow: `M${xs} 0L${W + fx} 0` + outline + 'Z',
        };
    }
    const outline = quarter(xs + fx, -fy, fx, fy, [-1, 0], [0, 1], n)
        + `L${W - R} 0` + quarter(W - R, R, R, R, [0, -1], [1, 0], n)
        + lower;
    const line = `M${xs} ${-fy}` + outline;
    const fill = `M${R} 0L${xs} 0L${xs} ${-fy}` + outline + tail + `L0 ${R}` + quarter(R, R, R, R, [-1, 0], [0, -1], n) + 'Z';
    const shadow = line + 'Z';
    return { fill, line, shadow };
}
function PeekPanel({ presenceRef, state, rect, edge, n, rk, mergeTop, focus, onPointerEnter, onPointerLeave, children }: {
    presenceRef: RefObject<HTMLDivElement>;
    state: string;
    rect: DOMRect;
    edge: number;
    n: number;
    rk: number;
    mergeTop: boolean;
    focus: boolean;
    onPointerEnter: () => void;
    onPointerLeave: () => void;
    children: ReactNode;
}) {
    const body = useRef<HTMLDivElement>(null);
    const [contentH, setContentH] = useState(0);
    useLayoutEffect(() => {
        const el = body.current;
        if (!el)
            return;
        const measure = () => setContentH(el.offsetHeight);
        measure();
        const ro = new ResizeObserver(measure);
        ro.observe(el);
        return () => ro.disconnect();
    }, []);
    useLayoutEffect(() => { const h = body.current?.offsetHeight ?? 0; if (h !== contentH)
        setContentH(h); });
    const W = PEEK_W, H = contentH > 0 ? contentH : rect.height;
    const R = 12 * rk, fx = FILLET.x, fy = FILLET.y;
    const xs = Math.max(R, Math.min(W - R - fx, edge - rect.left));
    const { fill, line, shadow } = peekShape(W, H, xs, R, fx, fy, n, mergeTop);
    const clipId = useId();
    const far = 200;
    return (<div ref={presenceRef} className="dev-peek" data-state={state} data-focus={focus ? '' : undefined} style={{ left: rect.left, top: rect.top, height: H, width: W, '--xs': `${xs}px`, '--fr': `${fy}px`, '--fx': `${fx}px` } as CSSProperties} onPointerEnter={onPointerEnter} onPointerLeave={onPointerLeave} aria-hidden>
      <svg className="dev-peek__bg" viewBox={`0 ${-fy} ${W} ${H + 2 * fy}`} aria-hidden>
        
        <clipPath id={clipId}>
          <rect x={xs} y={mergeTop ? 0 : -far} width={far * 4} height={far * 3}/>
        </clipPath>
        <g clipPath={`url(#${clipId})`}>
          <path className="dev-peek__shadow" d={shadow}/>
        </g>
        <path className="dev-peek__fill" d={fill}/>
        <path className="dev-peek__line" d={line}/>
      </svg>
      <div className="dev-peek__body" ref={body}>{children}</div>
      
      <span className="dev-peek__ring" aria-hidden/>
    </div>);
}
export interface PeekHandlers {
    onPointerEnter: (e: ReactPointerEvent<HTMLElement>) => void;
    onPointerLeave: () => void;
    onFocus: (e: FocusEvent<HTMLElement>) => void;
    onBlur: () => void;
}
