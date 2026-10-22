import { tr } from '../i18n';
import type { MouseEvent } from 'react';
import { Tooltip } from 'oink-ui';
import { Badge, CursorMark, GameMark, Glyph, OsLogo, PauseMark, Signal, SignalMark } from './icons';
import type { BadgeName, GlyphName } from './icons';
import type { Device, DeviceGroup } from './types';
import { faultReason, formatLastSeen, offlineReasonText, gameActive } from './data';
import type { ShareKind } from './data';
import { VolumeRow, hasVolume } from './VolumeRow';
import type { SpeakerAction } from './VolumeRow';
import type { PeekHandlers } from './peek';
import './devices.css';
type Tone = 'accent' | 'muted' | 'good' | 'bad' | 'warn';
interface Indicator {
    glyph: GlyphName;
    tone: Tone;
    badge?: BadgeName;
    label: string;
    tip?: string;
    waves?: number;
    muted?: boolean;
    disabled?: boolean;
}
export const volumeWaves = (v: number) => (v <= 0 ? 0 : v <= 33 ? 1 : v <= 66 ? 2 : v <= 100 ? 3 : 4);
export function indicators(d: Device, noExit = false): Indicator[] {
    const fault = (glyph: GlyphName, name: string, kind: ShareKind): Indicator => ({ glyph, tone: 'warn', badge: 'warning', label: tr('{0}不可用：{1}', name, faultReason(d, kind)) });
    const share = (glyph: GlyphName, name: string, s: 'shared' | 'local' | 'disabled' | 'fault' | 'unknown'): Indicator => (s === 'shared' ? { glyph, tone: 'good', badge: 'wifi', label: tr('{0}：共享', name) }
        : s === 'disabled' ? { glyph, tone: 'bad', badge: 'lock', label: tr('{0}：禁用', name) } : s === 'local' ? { glyph, tone: 'muted', label: tr('{0}：本地', name) }
            : s === 'unknown' ? { glyph, tone: 'warn', badge: 'warning', label: tr('{0}：不可用', name) } : fault(glyph, name, glyph as ShareKind));
    const keyboard: Indicator = d.keyboard === 'disabled'
        ? { glyph: 'keyboard', tone: 'bad', badge: 'lock', get label() {
                return tr('键盘：禁用');
            } }
        : share('keyboard', tr('键盘'), d.keyboard);
    const sp = d.speaker;
    const vol = (what: string) => ({
        waves: sp.volumeKnown === false ? 0 : volumeWaves(sp.volume), muted: sp.muted,
        label: tr('音频输出：{0}，音量 {1}', what, sp.volumeKnown === false ? tr('未知') : sp.volume),
        tip: tr('音频输出：{0}', what),
    });
    // Local: waves follow the device's own system volume (at most three); without an
    // exit in the group there is nowhere to forward to, so the icon is disabled.
    const out = sp.localOutput;
    const local = (): Indicator => ({ glyph: 'speaker', tone: 'muted', disabled: noExit,
        waves: out ? Math.min(3, volumeWaves(out.volume)) : 0, muted: out?.muted, badge: out?.muted ? 'mute' : undefined,
        label: !out ? tr('音频输出：本地') : out.muted ? tr('音频输出：本地，已静音') : tr('音频输出：本地，音量 {0}', out.volume) });
    const speaker: Indicator = sp.mode === 'unknown' ? { glyph: 'speaker', tone: 'warn', badge: 'warning', label: tr('音频输出：不可用') } : sp.mode === 'unused' ? local()
        : sp.mode === 'exit' ? { glyph: 'speaker', tone: 'accent', badge: sp.muted ? 'mute' : 'receive', ...vol(sp.muted ? tr('出口静音') : tr('作为出口')) }
            : sp.mode === 'fault' ? fault('speaker', tr('音频输出'), 'speaker')
                : (sp.waitingForExit ?? noExit) ? { glyph: 'speaker', tone: 'good', badge: 'warning', ...vol(sp.selectedExitUnavailable ? tr('所选出口不可用') : tr('无出口')) }
                    : { glyph: 'speaker', tone: 'good', badge: sp.muted ? 'mute' : 'wifi', ...vol(sp.muted ? tr('转发静音') : tr('正在转发')) };
    return [share('mouse', tr('鼠标'), d.mouse), keyboard, speaker];
}
function actionFor(d: Device, kind: ShareKind, exitName: string | null): string | null {
    if (kind === 'speaker') return d.speaker.mode !== 'unused' ? tr('点击选中这台设备，展开音量条') : exitName === null ? null : tr('点击转发音频到 {0}', exitName);
    if (d.actionability?.[kind] === 'open_permissions') return tr('点击前往设置 › 权限');
    if (d.actionability?.[kind] === 'unavailable') return null;
    if (d.unknown || d[kind] === 'unknown')
        return null;
    const fault = d[kind] === 'fault';
    if (fault)
        return d.group === 'self' && d.os === 'mac' && (kind === 'mouse' || kind === 'keyboard') ? tr('点击前往设置 › 权限') : null;
    switch (kind) {
        case 'keyboard':
            if (d.keyboard === 'disabled')
                return tr('点击解除禁用，回到{0}', d.keyboardBeforeLock === 'local' ? tr('本地') : tr('共享'));
            return tr('点击切换为{0}', d.keyboard === 'shared' ? tr('本地') : tr('共享'));
        default:
            return tr('点击切换为{0}', d[kind] === 'shared' ? tr('本地') : tr('共享'));
    }
}
function StatusIcon({ ind, action, double, onActivate, tabbable }: {
    ind: Indicator;
    action: string | null;
    double: boolean;
    onActivate?: (mode: 'single' | 'double') => void;
    tabbable: boolean;
}) {
    const body = (<>
      <Glyph name={ind.glyph} waves={ind.waves} muted={ind.muted}/>
      {ind.badge && <Badge name={ind.badge}/>}
    </>);
    if (!action || !onActivate) {
        return (<Tooltip interactive={false} content={ind.tip ?? ind.label}>
        <span className="dev-ico" data-tone={ind.tone} data-disabled={ind.disabled || undefined} role="img" aria-label={ind.label} aria-disabled={ind.disabled || undefined}>{body}</span>
      </Tooltip>);
    }
    const onClick = (e: MouseEvent<HTMLButtonElement>) => {
        if (double && e.detail === 2)
            onActivate('double');
        else if (e.detail <= 1)
            onActivate('single');
    };
    return (<Tooltip interactive={false} content={ind.tip ?? ind.label}>
      <button type="button" className="dev-ico dev-ico--btn ok-focusable" data-tone={ind.tone} aria-label={tr('{0}，{1}{2}', ind.label, action, double ? tr('（键盘：Shift + 回车相当于双击）') : '')} tabIndex={tabbable ? undefined : -1} onClick={onClick} onKeyDown={(e) => { if (double && e.key === 'Enter' && e.shiftKey) {
        e.preventDefault();
        onActivate('double');
    } }}>
        {body}
      </button>
    </Tooltip>);
}
function Connection({ d }: {
    d: Device;
}) {
    if (d.group === 'self')
        return null;
    const offline = d.group === 'offline';
    return (<span className="dev-conn" role="img" aria-label={offline ? offlineReasonText(d.offlineReason) : d.latency === null ? tr('延迟未知') : tr('延迟 {0} 毫秒', d.latency)}>
      <Signal latency={d.latency} offline={offline}/>
      
      {!offline && (<span className="dev-conn__ms">
          <span className="dev-conn__num">{d.latency === null ? '—' : Math.round(d.latency)}</span>{d.latency !== null && <small>ms</small>}
        </span>)}
    </span>);
}
function MetaLine({ d, now, onToggle, onSelect, interactive, tabbable, exitName }: {
    d: Device;
    now: number;
    onToggle?: (kind: ShareKind, mode: 'single' | 'double') => void;
    onSelect: () => void;
    interactive: boolean;
    tabbable: boolean;
    exitName: string | null;
}) {
    if (d.group === 'offline') {
        return <span className="dev__meta"><span className="dev__seen">{tr('上次在线 {0}', formatLastSeen(d.lastSeen, now))}</span></span>;
    }
    if (d.paused && d.pauseReason === 'input_backlog') {
        return <span className="dev__meta" title={tr('输入积压超过 5 秒，已暂停共享')}><span className="dev__seen">{tr('输入积压超过 5 秒，已暂停共享')}</span></span>;
    }
    return (<span className="dev__meta">
      <span className="dev__status">
        {indicators(d, exitName === null).map((ind) => {
            const kind = ind.glyph as ShareKind;
            // Audio: local mode forwards to the exit (the menu's "forward audio to …");
            // forwarding and exit select the device and open its volume slider.
            const activate = kind === 'speaker' && d.speaker.mode !== 'unused' ? onSelect : onToggle ? (mode: 'single' | 'double') => onToggle(kind, mode) : undefined;
            return (<StatusIcon key={ind.glyph} ind={ind} tabbable={tabbable} action={interactive ? actionFor(d, kind, exitName) : null} double={false} onActivate={activate}/>);
        })}
      </span>
    </span>);
}
const GROUPS: Array<{
    id: DeviceGroup;
    label: string;
}> = [
    { id: 'self', get label() {
            return tr('本机');
        } },
    { id: 'online', get label() {
            return tr('在线');
        } },
    { id: 'offline', get label() {
            return tr('离线');
        } },
];
function AccessibilityIndicator({ local, onGrant, tabbable }: { local: boolean; onGrant?: () => void; tabbable: boolean }) {
    const tip = local ? tr('缺少辅助功能权限，前往授权') : tr('缺少辅助功能权限，请在该设备上授权');
    const glyph = <svg viewBox="0 0 16 16" aria-hidden><circle cx="8" cy="8" r="5.5"/><path d="m4.1 4.1 7.8 7.8"/></svg>;
    return <Tooltip interactive={false} content={tip}>{local
        ? <button type="button" className="dev__accessibility ok-focusable" aria-label={tip} tabIndex={tabbable ? undefined : -1} onClick={onGrant}>{glyph}</button>
        : <span className="dev__accessibility" role="img" aria-label={tip}>{glyph}</span>}
    </Tooltip>;
}

export interface DeviceListProps {
    devices: Device[];
    selectedId: string | null;
    onSelect: (id: string) => void;
    onToggle?: (id: string, kind: ShareKind, mode: 'single' | 'double') => void;
    onSpeaker?: (id: string, a: SpeakerAction) => void;
    onPause?: (id: string, paused: boolean) => void;
    onGrantAccessibility?: () => void;
    growIds?: ReadonlySet<string>;
    /** Hidden screens per device id (a device whose sharing is off counts all of its screens). */
    hiddenScreens?: ReadonlyMap<string, number>;
    collapsed: boolean;
    now?: number;
}
export function DeviceList({ devices, selectedId, onSelect, onToggle, onSpeaker, onPause, onGrantAccessibility, growIds, hiddenScreens, collapsed, now = Date.now() }: DeviceListProps) {
    const exit = devices.find(d => d.group !== 'offline' && d.speaker.mode === 'exit');
    return (<div className="devs" data-collapsed={collapsed ? '' : undefined} role="group" aria-label={tr('设备')}>
      {GROUPS.map((g) => {
            const list = devices.filter((d) => d.group === g.id);
            if (!list.length)
                return null;
            const headingId = `devs-${g.id}`;
            return (<section key={g.id} className="devs__group" data-group={g.id} aria-labelledby={headingId}>
            <h3 className="devs__label" id={headingId}>
              <span>{g.label}</span>{g.id !== 'self' && <span className="devs__count">{list.length}</span>}
            </h3>
            <ul className="devs__list">
              {list.map((d) => (<li key={d.id}>
                  <DeviceItem onGrantAccessibility={onGrantAccessibility} d={d} now={now} selected={!collapsed && d.id === selectedId} collapsed={collapsed} exitName={exit?.name ?? null} onSelect={() => onSelect(d.id)} onToggle={onToggle ? (kind, mode) => onToggle(d.id, kind, mode) : undefined} onSpeaker={onSpeaker ? (a) => onSpeaker(d.id, a) : undefined} onPause={onPause ? (p) => onPause(d.id, p) : undefined} volumeOpen={!collapsed && d.id === selectedId} grow={collapsed && !!growIds?.has(d.id)} hiddenScreens={hiddenScreens?.get(d.id) ?? 0}/>
                </li>))}
            </ul>
          </section>);
        })}
    </div>);
}
export function DeviceItem({ d, now, selected, collapsed, exitName = null, onSelect, onToggle, onSpeaker, volumeOpen = false, grow = false, inPeek, hiddenScreens = 0, onGrantAccessibility }: {
    d: Device;
    now: number;
    selected: boolean;
    collapsed: boolean;
    onSelect: () => void;
    /** The group's audio exit; a local-mode audio icon forwards to it. */
    exitName?: string | null;
    onToggle?: (kind: ShareKind, mode: 'single' | 'double') => void;
    onSpeaker?: (a: SpeakerAction) => void;
    onPause?: (paused: boolean) => void;
    volumeOpen?: boolean;
    grow?: boolean;
    peek?: PeekHandlers;
    inPeek?: boolean;
    hiddenScreens?: number;
    onGrantAccessibility?: () => void;
}) {
    selected = selected && !collapsed;
    const summary = d.group === 'offline' ? tr('{0}，离线，上次在线 {1}', d.name, formatLastSeen(d.lastSeen, now)) : d.group === 'online' ? tr('{0}，在线，延迟 {1} ms', d.name, d.latency === null ? tr("未知") : d.latency) : tr('{0}，本机', d.name);
    const pausedNote = d.group !== 'offline' && d.paused ? tr('，已暂停') : '';
    const screenOff = d.group !== 'offline' && d.screen === 'disabled';
    const game = d.group !== 'offline' && gameActive(d) && (d.cursor === 'local' || d.cursor === 'locked');
    const cursorNote = d.group !== 'offline' && (d.cursor === 'local' || d.cursor === 'locked') ? (d.cursor === 'locked' ? tr('，指针锁定在这里') : tr('，指针在这里')) : '';
    return (<div aria-busy={d.pending || undefined} className="dev" data-ctx={`device:${d.id}`} data-group={d.group} data-current={selected ? '' : undefined} data-grow={grow && hasVolume(d) ? '' : undefined}>
      <Tooltip interactive={false} placement="right" disabled={!collapsed} content={d.group === 'online' ? `${d.name} · ${d.latency === null ? tr("延迟未知") : `${Math.round(d.latency)} ms`}` : d.name}>
      <button type="button" className="dev__hit ok-focusable" aria-current={selected ? 'true' : undefined} aria-disabled={d.group === 'offline' ? 'true' : undefined} aria-label={summary + pausedNote + (screenOff ? tr('，屏幕已禁用') : '') + (game ? tr('，游戏模式已开启') : '') + cursorNote} tabIndex={inPeek ? -1 : undefined} onClick={onSelect}/>
      </Tooltip>
      <span className="dev__os">
        <OsLogo os={d.os}/>
        
        
        {d.group !== 'self' && <SignalMark latency={d.latency} offline={d.group === 'offline'} shown={collapsed && !(d.group === 'online' && d.paused)}/>}
        {d.group !== 'offline' && <PauseMark shown={!!d.paused}/>}
        
        {game ? <GameMark tooltip={!collapsed}/>
                : d.group !== 'offline' && (d.cursor === 'local' || d.cursor === 'locked') && <CursorMark locked={d.cursor === 'locked'} tooltip={!collapsed}/>}
      </span>
      <span className="dev__text">
        <span className="dev__title"><span className="dev__name">{d.name}</span>{d.accessibilityMissing ? <AccessibilityIndicator local={d.group === 'self'} onGrant={onGrantAccessibility} tabbable={!collapsed && !inPeek}/> : hiddenScreens > 0 && <Tooltip interactive={false} content={tr('{0} 块屏幕已隐藏', hiddenScreens)}><span className="dev__hidden" role="img" aria-label={tr('{0} 块屏幕已隐藏', hiddenScreens)}><svg viewBox="0 0 16 16" aria-hidden><path d="M2 8s2.3-4.2 6-4.2S14 8 14 8s-2.3 4.2-6 4.2S2 8 2 8Z"/><circle cx="8" cy="8" r="1.8"/><path d="M3 3l10 10"/></svg></span></Tooltip>}</span>
        <MetaLine onSelect={onSelect} d={d} now={now} onToggle={onToggle} interactive={!collapsed} tabbable={!inPeek} exitName={exitName}/>
      </span>
      <Connection d={d}/>
      
      {onSpeaker && d.group !== 'offline' && <VolumeRow d={d} open={volumeOpen && !collapsed} tabbable={!inPeek} onSpeaker={onSpeaker}/>}
    </div>);
}
