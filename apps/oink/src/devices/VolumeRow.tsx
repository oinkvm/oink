import { tr } from '../i18n';
import { useEffect, useRef, useState } from 'react';
import type { CSSProperties, KeyboardEvent, PointerEvent as ReactPointerEvent } from 'react';
import { Button, Tooltip } from 'oink-ui';
import { Badge, Glyph } from './icons';
import { alignmentHaptic } from '../arrange/haptics';
import { pointerVolume, inVolumeDetent } from './volumeDetent';
import type { Device } from './types';
export type SpeakerAction = {
    type: 'volume';
    value: number;
    /** The pointer was released: send without waiting out the live-update spacing. */
    final?: boolean;
    /** Only pointer-drag positions opt in to deferred disk persistence. */
    dragging?: boolean;
} | {
    type: 'mute';
};
export type MicrophoneAction = SpeakerAction | 'receive' | 'install' | 'shared' | 'local';
export const hasVolume = (d: Device) =>
    (d.speaker.mode === 'forwarding' || d.speaker.mode === 'exit') &&
    d.speaker.volumeKnown !== false && d.speaker.mutedKnown !== false &&
    Number.isFinite(d.speaker.volume);
export const speakerMax = (d: Device) => (d.speaker.mode === 'exit' ? 100 : 200);
export function VolumeRow({ d, open, tabbable, disabled = false, onSpeaker, microphone = false }: {
    d: Device;
    microphone?: boolean;
    open: boolean;
    tabbable: boolean;
    disabled?: boolean;
    onSpeaker: (a: SpeakerAction) => void;
}) {
    const sp = microphone ? {
        mode: 'forwarding' as const, volume: d.microphone?.volume ?? 0,
        muted: d.microphone?.muted ?? false, canSetVolume: d.microphone?.canSetVolume ?? false,
        volumeKnown: true, mutedKnown: true,
    } : d.speaker;
    disabled = disabled || sp.canSetVolume === false || sp.volumeKnown === false || sp.mutedKnown === false;
    const live = sp.mode === 'forwarding' || sp.mode === 'exit';
    const exit = sp.mode === 'exit';
    const max = microphone ? 200 : speakerMax(d);
    const [armed, setArmed] = useState(false);
    useEffect(() => {
        const r = requestAnimationFrame(() => setArmed(true));
        const t = window.setTimeout(() => setArmed(true), 50);
        return () => { cancelAnimationFrame(r); window.clearTimeout(t); };
    }, []);
    if (microphone ? d.microphone?.mode !== 'source' : !hasVolume(d)) return null;
    const shown = open && live && armed;
    return (<div className="dev__vol" data-open={shown ? '' : undefined} aria-hidden={!shown || undefined} ref={(el) => { if (el)
        el.inert = !shown; }}>
      <div>
        
        <div className="vol" data-tone={exit || microphone ? 'accent' : 'good'} role="group" aria-label={microphone ? tr('{0} 的麦克风音量', d.name) : tr('{0} 的音频输出音量', d.name)}>
          <Tooltip interactive={false} content={sp.muted ? tr('取消静音') : tr('静音')}>
            <Button variant="ghost" size="sm" iconOnly className="vol__btn" aria-label={sp.muted ? tr('取消静音') : tr('静音')} aria-pressed={sp.muted} tabIndex={tabbable ? undefined : -1} disabled={disabled} onClick={() => onSpeaker({ type: 'mute' })}>
              <span className="vol__glyph" aria-hidden>
                <Glyph name={microphone ? "microphone" : "speaker"} waves={3} muted={sp.muted}/>
                {sp.muted && <Badge name="mute"/>}
              </span>
            </Button>
          </Tooltip>
          {<VolumeMeter value={Math.min(sp.volume, max)} max={max} muted={sp.muted} tabbable={tabbable} disabled={disabled} onChange={(value, final, dragging) => onSpeaker({ type: 'volume', value, final, dragging })}/>}
        </div>
      </div>
    </div>);
}
function VolumeMeter({ value, max, muted, tabbable, disabled, onChange }: {
    value: number;
    max: number;
    muted: boolean;
    tabbable: boolean;
    disabled: boolean;
    onChange: (v: number, final?: boolean, dragging?: boolean) => void;
}) {
    const ref = useRef<HTMLDivElement>(null);
    const [active, setActive] = useState(false);
    const [draft,setDraft] = useState<number|null>(null);
    const lastDraft = useRef(value);
    const detent = useRef(false);
    // Every position applies at once (the caller coalesces sends); keys and wheel
    // ticks build on the last position sent, not on a round trip still on its way.
    const latest = useRef(value);
    useEffect(() => { latest.current = value; }, [value]);
    const live = useRef({ disabled, max, onChange });
    live.current = { disabled, max, onChange };
    const apply = (next: number) => {
        next = Math.max(0, Math.min(live.current.max, next));
        if (next === latest.current) return;
        latest.current = next;
        live.current.onChange(next);
    };
    const setFrom = (clientX: number) => {
        const r = ref.current!.getBoundingClientRect();
        const raw = Math.round(Math.max(0, Math.min(1, (clientX - r.left) / r.width)) * max);
        const entered = inVolumeDetent(raw, max);
        if (entered && !detent.current) alignmentHaptic();
        detent.current = entered;
        const next = pointerVolume(raw, max);
        if (next === lastDraft.current) return;
        lastDraft.current=next; setDraft(next);
        latest.current = next;
        onChange(next, false, true);
    };
    const onPointerDown = (e: ReactPointerEvent<HTMLDivElement>) => {
        if (e.button !== 0 || disabled)
            return;
        e.currentTarget.setPointerCapture(e.pointerId);
        setActive(true);
        lastDraft.current = NaN;
        detent.current = false;
        setFrom(e.clientX);
    };
    const end = () => { detent.current = false; if(active) onChange(lastDraft.current, true); setActive(false); setDraft(null); };
    const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
        if (disabled)
            return;
        const step = e.shiftKey ? 10 : 1;
        const base = latest.current;
        const next = ({ ArrowRight: base + step, ArrowUp: base + step, ArrowLeft: base - step, ArrowDown: base - step, PageUp: base + 10, PageDown: base - 10, Home: 0, End: max } as Record<string, number>)[e.key];
        if (next == null)
            return;
        e.preventDefault();
        apply(next);
    };
    // React registers wheel listeners as passive; this one must cancel the sidebar scroll.
    useEffect(() => {
        const el = ref.current;
        if (!el) return;
        const onWheel = (e: WheelEvent) => {
            if (live.current.disabled) return;
            const delta = -(Math.abs(e.deltaY) >= Math.abs(e.deltaX) ? e.deltaY : e.deltaX);
            if (!delta) return;
            e.preventDefault();
            apply(latest.current + Math.sign(delta) * (e.shiftKey ? 10 : 1));
        };
        el.addEventListener('wheel', onWheel, { passive: false });
        return () => el.removeEventListener('wheel', onWheel);
    }, []);
    return (<div ref={ref} className={`ok-meter vol__meter${active ? ' is-active is-tracking' : ''}`} data-muted={muted ? '' : undefined} role="slider" tabIndex={tabbable && !disabled ? 0 : -1} aria-disabled={disabled || undefined} aria-label={tr('音量')} aria-valuemin={0} aria-valuemax={max} aria-valuenow={draft ?? value} aria-valuetext={`${draft ?? value}%`} style={{ '--v': (draft ?? value) / max } as CSSProperties} onPointerDown={onPointerDown} onPointerMove={(e) => { if (active)
        setFrom(e.clientX); }} onPointerUp={end} onPointerCancel={end} onLostPointerCapture={end} onKeyDown={onKeyDown}>
      <span className="ok-meter__fill"/>
      <span className="ok-meter__value">{draft ?? value}%</span>
      
      <span className="ok-meter__value vol__value-on" aria-hidden>{draft ?? value}%</span>
    </div>);
}
