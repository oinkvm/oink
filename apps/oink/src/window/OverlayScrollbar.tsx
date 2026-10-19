import { useLayoutEffect, useRef, useState } from 'react';
import type { PointerEvent as ReactPointerEvent, RefObject } from 'react';
export function OverlayScrollbar({ target, disabled }: {
    target: RefObject<HTMLElement | null>;
    disabled?: boolean;
}) {
    const [m, setM] = useState({ overflow: false, top: 0, size: 0 });
    const [dragging, setDragging] = useState(false);
    const drag = useRef<{
        y: number;
        scroll: number;
        ratio: number;
    } | null>(null);
    const track = useRef<HTMLDivElement>(null);
    useLayoutEffect(() => {
        const el = target.current;
        if (!el)
            return;
        const measure = () => {
            const { scrollTop, scrollHeight, clientHeight } = el;
            const overflow = scrollHeight > clientHeight + 1;
            const trackH = track.current?.clientHeight || clientHeight;
            const size = overflow ? Math.max(24, (trackH * clientHeight) / scrollHeight) : 0;
            const top = overflow ? (scrollTop / (scrollHeight - clientHeight)) * (trackH - size) : 0;
            setM((p) => (p.overflow === overflow && Math.abs(p.top - top) < 0.25 && Math.abs(p.size - size) < 0.25 ? p : { overflow, top, size }));
        };
        measure();
        el.addEventListener('scroll', measure, { passive: true });
        const ro = new ResizeObserver(measure);
        ro.observe(el);
        for (const child of Array.from(el.children))
            ro.observe(child);
        return () => { el.removeEventListener('scroll', measure); ro.disconnect(); };
    }, [target]);
    const onThumbDown = (e: ReactPointerEvent) => {
        const el = target.current;
        if (!el || e.button !== 0)
            return;
        e.preventDefault();
        e.stopPropagation();
        const trackH = track.current?.clientHeight || el.clientHeight;
        const ratio = (el.scrollHeight - el.clientHeight) / Math.max(1, trackH - m.size);
        drag.current = { y: e.clientY, scroll: el.scrollTop, ratio };
        (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
        setDragging(true);
    };
    const onThumbMove = (e: ReactPointerEvent) => {
        const d = drag.current, el = target.current;
        if (!d || !el)
            return;
        el.scrollTop = d.scroll + (e.clientY - d.y) * d.ratio;
    };
    const onThumbUp = () => { drag.current = null; setDragging(false); };
    const onTrackDown = (e: ReactPointerEvent) => {
        const el = target.current, tr = track.current;
        if (!el || !tr || e.button !== 0)
            return;
        const y = e.clientY - tr.getBoundingClientRect().top;
        el.scrollBy({ top: (y < m.top ? -1 : 1) * el.clientHeight * 0.9, behavior: 'smooth' });
    };
    if (disabled)
        return null;
    return (<div ref={track} className="ovs" aria-hidden data-overflow={m.overflow ? '' : undefined} data-dragging={dragging ? '' : undefined} onPointerDown={onTrackDown}>
      <div className="ovs__thumb" style={{ height: m.size, transform: `translateY(${m.top}px)` }} onPointerDown={onThumbDown} onPointerMove={onThumbMove} onPointerUp={onThumbUp} onPointerCancel={onThumbUp} onLostPointerCapture={onThumbUp}/>
    </div>);
}
