import { tr } from '../i18n';
import type { ReactNode } from 'react';
import { Tooltip } from 'oink-ui';
import type { Device, DeviceOS } from './types';
import { ANDROID_CROWN, ANDROID_CROWN_STROKE, ANDROID_CROWN_VIEWBOX, ANDROID_EYES, ANDROID_HEAD, ANDROID_HEAD_VIEWBOX } from './androidHead';
import { latencyTone, signalLevel } from './latency';
const APPLE = <>
      <path d="M15.6 1.5c.1 1.3-.4 2.6-1.2 3.5-.8.9-2 1.6-3.1 1.5-.1-1.3.5-2.6 1.2-3.4.8-.9 2.1-1.5 3.1-1.6z"/>
      <path d="M19.6 17c-.5 1.2-.8 1.7-1.5 2.8-1 1.5-2.3 3.3-4 3.3-1.5 0-1.9-1-3.9-1s-2.5 1-4 1c-1.7 0-2.9-1.7-3.9-3.1C-.4 15.9-.6 11.1 1 8.6c1.2-1.8 3-2.8 4.8-2.8s2.9 1 4.4 1c1.4 0 2.3-1 4.4-1 1.6 0 3.2.9 4.4 2.3-3.9 2.1-3.3 7.6.6 8.9z"/>
    </>;
export function OsLogo({ os, formFactor, privileged = false }: {
    os: DeviceOS;
    formFactor?: Device['formFactor'];
    /** Android privileged mode is on and running: the crowned head. */
    privileged?: boolean;
}) {
    // Same badges as the phone app: the official Android robot head, and the
    // Apple logo with an iPhone or iPad corner (docs/native-rewrite.md sizes,
    // scaled from its 28 px apple to the 20 px apple of this row).
    // The crown is two layers: the path with a round stroke in the edge colour
    // (the whole footprint), then the bare path in the crown colour on top.
    if (os === 'android' && privileged) return <svg className="os-logo" data-os="android" data-privileged="" viewBox={ANDROID_CROWN_VIEWBOX} role="img" aria-label={tr('Android，特权模式已开启')}>
        <path d={ANDROID_HEAD}/><path className="os-logo__knock" d={ANDROID_EYES}/>
        <path className="os-logo__crown-edge" strokeWidth={ANDROID_CROWN_STROKE} strokeLinejoin="round" d={ANDROID_CROWN}/>
        <path className="os-logo__crown" d={ANDROID_CROWN}/>
    </svg>;
    if (os === 'android') return <svg className="os-logo" data-os="android" viewBox={ANDROID_HEAD_VIEWBOX} aria-hidden><path d={ANDROID_HEAD}/><path className="os-logo__knock" d={ANDROID_EYES}/></svg>;
    if (os === 'ios') {
        const ipad = formFactor === 'tablet';
        return <span className="os-apple" data-device={ipad ? 'ipad' : 'iphone'} aria-hidden>
            <svg className="os-logo" data-os="mac" viewBox="0 0 24 24">{APPLE}</svg>
            <svg className="os-apple__corner" viewBox="0 0 16 20"><rect x={ipad ? 1 : 3} y="1" width={ipad ? 14 : 10} height="18" rx="2.4"/><path d={ipad ? 'M6 17h4' : 'M6 3h4M6 17h4'}/></svg>
        </span>;
    }
    if (os === 'other' || os === 'linux') return <svg className="os-logo" viewBox="0 0 24 24" aria-hidden><rect x="2" y="3" width="20" height="14" rx="2" fill="none" stroke="currentColor" strokeWidth="2"/><path d="M8 22h8M12 17v5" stroke="currentColor" strokeWidth="2"/></svg>;
    if (os === 'win') {
        return (<svg className="os-logo" data-os="win" viewBox="0 0 88 88" aria-hidden>
        <path d="M0 12.4 35.7 7.5v34.4L0 42.2zM35.7 45.9v34.5L0 75.5V45.7zM40 6.9 87.3 0v41.5L40 41.9zM87.3 46.3v41.3L40 80.9V46.2z"/>
      </svg>);
    }
    return (<svg className="os-logo" data-os="mac" viewBox="0 0 24 24" aria-hidden>
      {APPLE}
    </svg>);
}
export function UpgradeMark() {
    return <span className="os-upgrade" role="img" aria-label={tr('需要升级')}><svg viewBox="0 0 16 16" aria-hidden><circle cx="8" cy="8" r="6.2"/><path d="M8 4.3v5.2M5.8 7.3 8 9.5l2.2-2.2M5.4 12h5.2"/></svg></span>;
}
export function CursorMark({ locked, tooltip = true }: {
    locked: boolean;
    tooltip?: boolean;
}) {
    return (<Tooltip interactive={false} content={locked ? tr('指针：锁定') : tr('指针：在这台设备上')} disabled={!tooltip}>
    <span className="os-cursor" data-locked={locked ? '' : undefined} role="img" aria-label={locked ? tr('指针锁定在这台设备上') : tr('指针在这台设备上')}>
      {locked ? (<svg viewBox="-3 -3 20 20" aria-hidden>
          <path className="os-cursor__shackle" d="M4.3 6.6V4.6a2.7 2.7 0 0 1 5.4 0v2"/>
          <rect x="2.4" y="6.2" width="9.2" height="7" rx="1.8"/>
          <circle className="os-cursor__keyhole" cx="7" cy="9.7" r="1"/>
        </svg>) : (<svg viewBox="-3 -3 20 20" aria-hidden>
          <path d="M13 1 1.4 6l4.9 1.7L8 12.6z"/>
        </svg>)}
    </span>
    </Tooltip>);
}
export function GameMark({ tooltip = true }: {
    tooltip?: boolean;
}) {
    return (<Tooltip interactive={false} content={tr('游戏模式：开启')} disabled={!tooltip}>
      <span className="os-game" role="img" aria-label={tr('游戏模式已开启')}>
        <svg viewBox="-1.5 -1.5 19 19" aria-hidden>
          <path className="os-game__knock" d={GAMEPAD}/>
          <path className="os-game__body" d={GAMEPAD}/>
          <path className="os-game__cut" d="M4.7 6.9v2.2M3.6 8h2.2"/>
          <circle className="os-game__dot" cx="10.9" cy="7.3" r=".85"/>
          <circle className="os-game__dot" cx="12.3" cy="8.8" r=".85"/>
        </svg>
      </span>
    </Tooltip>);
}
const GAMEPAD = 'M5.1 4.3h5.8a3.6 3.6 0 0 1 3.5 2.8l.9 4.1a1.9 1.9 0 0 1-3.2 1.7l-1.6-1.8H5.5l-1.6 1.8a1.9 1.9 0 0 1-3.2-1.7l.9-4.1a3.6 3.6 0 0 1 3.5-2.8z';
export function SignalMark({ latency, offline, shown = true }: {
    latency?: number | null;
    offline?: boolean;
    shown?: boolean;
}) {
    return (<span className="os-signal" data-shown={shown ? '' : undefined} role="img" aria-hidden={!shown || undefined} aria-label={offline ? tr('离线') : tr('延迟 {0} 毫秒', latency)}>
      <Signal latency={latency} offline={offline}/>
    </span>);
}
export function PauseMark({ shown }: {
    shown: boolean;
}) {
    return (<span className="os-pause" data-shown={shown ? '' : undefined} role="img" aria-hidden={!shown || undefined} aria-label={tr('已暂停')}>
      <PauseGlyph knockout/>
    </span>);
}
export function PauseGlyph({ knockout = false }: { knockout?: boolean }) {
    return (<svg viewBox={knockout ? '-2 -2 16 16' : '0 0 12 12'} aria-hidden="true" focusable="false">
        {knockout && <circle className="os-pause__knock" cx="6" cy="6" r="7.5"/>}
        <circle className="os-pause__dot" cx="6" cy="6" r="6"/>
        <path className="os-pause__bars" d="M4.6 3.7v4.6M7.4 3.7v4.6"/>
      </svg>);
}
export { signalLevel };
export function Signal({ latency, offline }: {
    latency?: number | null;
    offline?: boolean;
}) {
    const level = offline || latency == null ? 0 : signalLevel(latency);
    const tone = offline ? 'off' : latencyTone(latency ?? null) ?? 'off';
    const bars = [
        { x: 1, y: 8, h: 4 },
        { x: 5.5, y: 5, h: 7 },
        { x: 10, y: 2, h: 10 },
    ];
    return (<svg className="dev-signal" data-tone={tone} viewBox="0 0 14 13" aria-hidden>
      {bars.map((b, i) => <rect key={i} x={b.x} y={b.y} width="3" height={b.h} rx=".9" data-on={i < level ? '' : undefined}/>)}
      {offline && <path className="dev-signal__slash" d="M1.5 1.5 12.5 12"/>}
    </svg>);
}
export type GlyphName = 'screen' | 'game' | 'mouse' | 'keyboard' | 'clipboard' | 'speaker' | 'microphone';
export function Glyph({ name, waves = 0, muted = false }: {
    name: GlyphName;
    waves?: number;
    muted?: boolean;
}) {
    switch (name) {
        case 'screen':
            return (<svg viewBox="0 0 16 16" aria-hidden>
          <rect x="1.7" y="2.7" width="12.6" height="8.6" rx="1.7"/>
          <path d="M5.8 13.8h4.4M8 11.3v2.5"/>
        </svg>);
        case 'game':
            return (<svg viewBox="0 0 16 16" aria-hidden>
          <path d={GAMEPAD}/>
          <path d="M4.7 6.9v2.2M3.6 8h2.2"/>
          <circle className="fill" cx="10.9" cy="7.3" r=".7"/>
          <circle className="fill" cx="12.3" cy="8.8" r=".7"/>
        </svg>);
        case 'mouse':
            return (<svg viewBox="0 0 16 16" aria-hidden>
          <rect x="4.2" y="1.7" width="7.6" height="12.6" rx="3.8"/>
          <path d="M8 4.2v2.4"/>
        </svg>);
        case 'keyboard':
            return (<svg viewBox="0 0 16 16" aria-hidden>
          <rect x="1.2" y="3.7" width="13.6" height="8.6" rx="1.7"/>
          <path d="M4 6.7h.01M6.7 6.7h.01M9.3 6.7h.01M12 6.7h.01M5.2 9.4h5.6"/>
        </svg>);
        case 'clipboard':
            return (<svg viewBox="0 0 16 16" aria-hidden>
          <path d="M5.6 2.9H4.4a1.7 1.7 0 0 0-1.7 1.7v8.2a1.7 1.7 0 0 0 1.7 1.7h7.2a1.7 1.7 0 0 0 1.7-1.7V4.6a1.7 1.7 0 0 0-1.7-1.7h-1.2"/>
          <rect x="5.6" y="1.6" width="4.8" height="2.6" rx="1"/>
        </svg>);
        case 'microphone':
            // Match the mouse silhouette bounds (7.6 × 12.6) in the shared 16px viewport.
            return <svg viewBox="0 0 16 16" aria-hidden><rect x="5.7" y="1.7" width="4.6" height="8" rx="2.3"/><path d="M4.2 7.4v.7a3.8 3.8 0 0 0 7.6 0v-.7M8 11.9v2.4M5.9 14.3h4.2"/></svg>;
        case 'speaker': {
            // Muted keeps the level visible but dimmed; the caller swaps the corner
            // badge for a cross (Badge 'mute'), the same rule as the microphone.
            const arcs = [2.6, 4.4, 6.2, 8].slice(0, waves).map((r) => {
                const c = 6.6, a = Math.SQRT1_2 * r;
                return `M${(c + a).toFixed(2)} ${(8 - a).toFixed(2)}A${r} ${r} 0 0 1 ${(c + a).toFixed(2)} ${(8 + a).toFixed(2)}`;
            });
            return (<svg viewBox="0 0 16 16" aria-hidden>
          <path className="fill" d="M1.5 5.8h2.3L7.2 3v10L3.8 10.2H1.5z"/>
          {arcs.map((d, i) => <path key={i} d={d} className={muted ? 'dim' : undefined}/>)}
        </svg>);
        }
    }
}
export type BadgeName = 'wifi' | 'ban' | 'warning' | 'receive' | 'send' | 'mute';
export function Badge({ name }: {
    name: BadgeName;
}) {
    let body: ReactNode;
    switch (name) {
        case 'wifi':
            body = <><path d="M1.6 4.6a4.8 4.8 0 0 1 6.8 0"/><path d="M3.2 6.2a2.5 2.5 0 0 1 3.6 0"/><circle className="fill" cx="5" cy="8" r=".95"/></>;
            break;
        case 'ban':
            body = <><circle cx="5" cy="5" r="3.7"/><path d="M2.4 2.4l5.2 5.2"/></>;
            break;
        case 'warning':
            body = <><path className="fill warn-body" d="M5 1 9.4 8.8H.6z"/><path className="warn-mark" d="M5 3.9v2.2M5 7.5v.01"/></>;
            break;
        case 'receive':
            body = <><path d="M5 1.2v4.6M3 3.9 5 5.9l2-2"/><path d="M1.6 6.6v1.4h6.8V6.6"/></>;
            break;
        case 'send':
            // The exit badge ('receive') mirrored: the sharing microphone sends out.
            body = <><path d="M5 5.9V1.3M3 3.3 5 1.3l2 2"/><path d="M1.6 6.6v1.4h6.8V6.6"/></>;
            break;
        case 'mute':
            body = <path d="M2.6 2.6l4.8 4.8M7.4 2.6l-4.8 4.8"/>;
            break;
    }
    return (<svg className="dev-badge" data-badge={name} viewBox="-1 -1 12 12" aria-hidden>
      <circle className="dev-badge__knock" cx="5" cy="5" r="6"/>
      {body}
    </svg>);
}
