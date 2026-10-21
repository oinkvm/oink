import { tr } from '../i18n';
import { useEffect, useRef, useState } from 'react';
import type { CSSProperties, KeyboardEvent, PointerEvent as ReactPointerEvent } from 'react';
import { Button, Tooltip } from 'oink-ui';
import { Glyph, Slash } from './icons';
import type { Device } from './types';
export type SpeakerAction = {
    type: 'volume';
    value: number;
} | {
    type: 'mute';
};
export const hasVolume = (d: Device) =>
    (d.speaker.mode === 'forwarding' || d.speaker.mode === 'exit') &&
    d.speaker.volumeKnown !== false && d.speaker.mutedKnown !== false &&
    Number.isFinite(d.speaker.volume);
export const speakerMax = (d: Device) => (d.speaker.mode === 'exit' ? 100 : 200);
export function VolumeRow({ d, open, tabbable, disabled = false, onSpeaker }: {
    d: Device;
    open: boolean;
    tabbable: boolean;
    disabled?: boolean;
    onSpeaker: (a: SpeakerAction) => void;
}) {
    const sp = d.speaker;
    disabled = disabled || d.pending === true || sp.canSetVolume === false || sp.volumeKnown === false || sp.mutedKnown === false;
    const live = sp.mode === 'forwarding' || sp.mode === 'exit';
    const exit = sp.mode === 'exit';
    const max = speakerMax(d);
    const [armed, setArmed] = useState(false);
    useEffect(() => {
        const r = requestAnimationFrame(() => setArmed(true));
        const t = window.setTimeout(() => setArmed(true), 50);
        return () => { cancelAnimationFrame(r); window.clearTimeout(t); };
    }, []);
    if (!hasVolume(d)) return null;
    const shown = open && live && armed;
    return (<div className="dev__vol" data-open={shown ? '' : undefined} aria-hidden={!shown || undefined} ref={(el) => { if (el)
        el.inert = !shown; }}>
      <div>
        
        <div className="vol" data-tone={exit ? 'accent' : 'good'} role="group" aria-label={tr('{0} 的音频输出音量', d.name)}>
          <Tooltip interactive={false} content={sp.muted ? tr('取消静音') : tr('静音')}>
            <Button variant="ghost" size="sm" iconOnly className="vol__btn" aria-label={sp.muted ? tr('取消静音') : tr('静音')} aria-pressed={sp.muted} tabIndex={tabbable ? undefined : -1} disabled={disabled} onClick={() => onSpeaker({ type: 'mute' })}>
              <span className="vol__glyph" aria-hidden>
                <Glyph name="speaker" waves={sp.muted ? 2 : 3}/>
                {sp.muted && <Slash />}
              </span>
            </Button>
          </Tooltip>
          {<VolumeMeter value={Math.min(sp.volume, max)} max={max} muted={sp.muted} tabbable={tabbable} disabled={disabled} onChange={(value) => onSpeaker({ type: 'volume', value })}/>}
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
    onChange: (v: number) => void;
}) {
    const ref = useRef<HTMLDivElement>(null);
    const [active, setActive] = useState(false);
    const [draft,setDraft] = useState<number|null>(null);
    const lastDraft = useRef(value);
    const setFrom = (clientX: number) => {
        const r = ref.current!.getBoundingClientRect();
        const next = Math.round(Math.max(0, Math.min(1, (clientX - r.left) / r.width)) * max);
        lastDraft.current=next; setDraft(next);
    };
    const onPointerDown = (e: ReactPointerEvent<HTMLDivElement>) => {
        if (e.button !== 0 || disabled)
            return;
        e.currentTarget.setPointerCapture(e.pointerId);
        setActive(true);
        setFrom(e.clientX);
    };
    const end = () => { if(active) onChange(lastDraft.current); setActive(false); setDraft(null); };
    const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
        if (disabled)
            return;
        const step = e.shiftKey ? 10 : 1;
        const next = ({ ArrowRight: value + step, ArrowUp: value + step, ArrowLeft: value - step, ArrowDown: value - step, PageUp: value + 10, PageDown: value - 10, Home: 0, End: max } as Record<string, number>)[e.key];
        if (next == null)
            return;
        e.preventDefault();
        onChange(Math.max(0, Math.min(max, next)));
    };
    return (<div ref={ref} className={`ok-meter vol__meter${active ? ' is-active is-tracking' : ''}`} data-muted={muted ? '' : undefined} role="slider" tabIndex={tabbable && !disabled ? 0 : -1} aria-disabled={disabled || undefined} aria-label={tr('音量')} aria-valuemin={0} aria-valuemax={max} aria-valuenow={draft ?? value} aria-valuetext={`${draft ?? value}%`} style={{ '--v': (draft ?? value) / max } as CSSProperties} onPointerDown={onPointerDown} onPointerMove={(e) => { if (active)
        setFrom(e.clientX); }} onPointerUp={end} onPointerCancel={end} onLostPointerCapture={end} onKeyDown={onKeyDown}>
      <span className="ok-meter__fill"/>
      <span className="ok-meter__value">{draft ?? value}%</span>
      
      <span className="ok-meter__value vol__value-on" aria-hidden>{draft ?? value}%</span>
    </div>);
}
