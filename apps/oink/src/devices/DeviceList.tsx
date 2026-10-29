import { tr, problemText } from '../i18n';
import { useRef, type MouseEvent, type ReactNode } from 'react';
import { Tooltip } from 'oink-ui';
import { Badge, CursorMark, GameMark, Glyph, OsLogo, PauseMark, Signal, SignalMark } from './icons';
import type { BadgeName, GlyphName } from './icons';
import type { Device, DeviceGroup } from './types';
import { canDisableInput, faultReason, formatLastSeen, offlineReasonText, gameActive } from './data';
import type { ShareKind } from './data';
import { VolumeRow, hasVolume } from './VolumeRow';
import type { SpeakerAction, MicrophoneAction } from './VolumeRow';
import type { PeekHandlers } from './peek';
import { PermissionHint, type PermissionHintKind } from '../permissions/PermissionHint';
import './devices.css';
import { useCardLatency } from './useCardLatency';
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
export function microphoneMode(d: Device): 'local' | 'receiver' | 'source' {
    return d.microphone?.mode==='source' ? 'source' : d.microphone?.virtualReady && (d.microphone.mode==='receiver' || d.microphone.mode==='idle') ? 'receiver' : 'local';
}
function microphoneState(d: Device): string {
    const mic = d.microphone, mode = microphoneMode(d);
    if (mode === 'local') return tr('本地');
    if (mic?.fault && mic.fault.code !== 'none') return problemText(mic.fault.code);
    // Capture/consumer/receive activity, preparation, sleep and pause are
    // internal audio states: cards show only the role, mute, waiting for a
    // source, permission and faults.
    if (mode === 'source') {
        if (!mic?.permissionGranted) return problemText('permission_denied');
        return mic.muted ? tr('发送中，已静音') : tr('发送');
    }
    if (mic?.waitingForSource) return tr('当前没有设备共享麦克风');
    return tr('接收');
}
export const volumeWaves = (v: number) => (v <= 0 ? 0 : v <= 33 ? 1 : v <= 66 ? 2 : v <= 100 ? 3 : 4);
export function indicators(d: Device, noExit = false): Indicator[] {
    const fault = (glyph: GlyphName, name: string, kind: ShareKind): Indicator => ({ glyph, tone: 'warn', badge: 'warning', label: tr('{0}不可用：{1}', name, faultReason(d, kind)) });
    const share = (glyph: GlyphName, name: string, s: 'shared' | 'local' | 'disabled' | 'fault' | 'unknown'): Indicator => (s === 'shared' ? { glyph, tone: 'good', badge: 'wifi', label: tr('{0}：共享', name) }
        : s === 'disabled' ? { glyph, tone: 'bad', badge: 'ban', label: tr('{0}：禁用', name) } : s === 'local' ? { glyph, tone: 'muted', label: tr('{0}：本地', name) }
            : s === 'unknown' ? { glyph, tone: 'warn', badge: 'warning', label: tr('{0}：不可用', name) } : fault(glyph, name, glyph as ShareKind));
    const keyboard: Indicator = d.keyboard === 'disabled'
        ? { glyph: 'keyboard', tone: 'bad', badge: 'ban', get label() {
                return tr('键盘：禁用');
            } }
        : share('keyboard', tr('键盘'), d.keyboard);
    const sp = d.speaker;
    const vol = (what: string) => ({
        waves: sp.volumeKnown === false ? 0 : volumeWaves(sp.volume), muted: sp.muted,
        label: tr('音频输出：{0}，音量 {1}', what, sp.volumeKnown === false ? tr('未知') : sp.volume),
        tip: tr('音频输出：{0}', what),
    });
    // Local waves follow the device's own system volume (at most three).
    // Routing remains available without an exit: a double click can create one.
    const out = sp.localOutput;
    const local = (): Indicator => ({ glyph: 'speaker', tone: 'muted', disabled: false,
        waves: out ? Math.min(3, volumeWaves(out.volume)) : 0, muted: out?.muted, badge: out?.muted ? 'mute' : undefined,
        label: !out ? tr('音频输出：本地') : out.muted ? tr('音频输出：本地，已静音') : tr('音频输出：本地，音量 {0}', out.volume) });
    const speaker: Indicator = sp.mode === 'unknown' ? { glyph: 'speaker', tone: 'warn', badge: 'warning', label: tr('音频输出：不可用') } : sp.mode === 'unused' ? local()
        : sp.mode === 'exit' ? { glyph: 'speaker', tone: 'accent', badge: sp.muted ? 'mute' : 'receive', ...vol(sp.muted ? tr('出口静音') : tr('作为出口')) }
            : sp.mode === 'fault' ? fault('speaker', tr('音频输出'), 'speaker')
                : (sp.waitingForExit ?? noExit) ? { glyph: 'speaker', tone: 'good', badge: 'warning', ...vol(sp.exitUnknown ? tr('等待音频出口') : sp.selectedExitUnavailable ? tr('所选出口不可用') : tr('无出口')) }
                    : { glyph: 'speaker', tone: 'good', badge: sp.muted ? 'mute' : 'send', ...vol(sp.muted ? tr('转发静音') : tr('正在转发')) };
    const mode = microphoneMode(d);
    const mic = d.microphone;
    const micFault = !!mic?.fault && mic.fault.code !== 'none';
    const microphone: Indicator = { glyph:'microphone', tone:micFault?'warn':mode==='source'?'accent':mode==='receiver'?'good':'muted',
        badge:micFault?'warning':mode==='source'?(mic?.muted?'mute':'send'):mode==='receiver'?(mic?.waitingForSource?'warning':'receive'):'mute',
        label:tr('麦克风：{0}', microphoneState(d)) };
    return [share('mouse', tr('鼠标'), d.mouse), keyboard, speaker, microphone];
}
function actionFor(d: Device, kind: ShareKind): string | null {
    if (kind === 'speaker') return tr('点击选中这台设备');
    if (d.actionability?.[kind] === 'open_permissions') return d.group==='self'?tr('点击前往设置 › 权限'):null;
    if (d.actionability?.[kind] === 'unavailable') return null;
    if (d.unknown || d[kind] === 'unknown')
        return null;
    const fault = d[kind] === 'fault';
    if (fault)
        return d.group === 'self' && d.os === 'mac' && (kind === 'mouse' || kind === 'keyboard') ? tr('点击前往设置 › 权限') : null;
    switch (kind) {
        case 'keyboard':
            if (d.keyboard === 'disabled')
                return tr('恢复共享');
            return d.keyboard === 'shared' ? tr('停止共享') : tr('切换为{0}', tr('共享'));
        default:
            if (d[kind] === 'disabled') return tr('恢复共享');
            return d[kind] === 'shared' ? tr('停止共享') : tr('切换为{0}', tr('共享'));
    }
}
export function audioTooltip(d: Device, kind: 'speaker' | 'microphone', exitName: string | null): string[] {
    let state: string, single: string, double: string | null;
    if (kind === 'speaker') {
        const sp = d.speaker;
        if (sp.mode === 'unknown' || sp.mode === 'fault') {
            state = tr('扬声器：不可用，{0}', faultReason(d, 'speaker')); single = tr('选中设备'); double = null;
        } else if (sp.mode === 'exit') {
            state = tr('扬声器：出口');
            single = sp.muted ? tr('取消静音') : tr('静音'); double = tr('停止共享');
        } else if (sp.mode === 'forwarding') {
            state = exitName ? tr('扬声器：转发到 {0}', exitName)
                : tr('扬声器：不可用，{0}', sp.selectedExitUnavailable ? tr('所选出口不可用') : sp.exitUnknown ? tr('等待音频出口') : tr('无出口'));
            single = sp.muted ? tr('取消静音') : tr('静音'); double = tr('停止共享');
        } else {
            state = tr('扬声器：本地');
            single = tr('发送音频'); double = tr('设为音频出口');
        }
    } else {
        const mode = microphoneMode(d);
        const muted = mode==='source' && d.microphone?.muted;
        const fault = d.microphone?.fault && d.microphone.fault.code !== 'none' ? d.microphone.fault.code
            : mode==='source' && !d.microphone?.permissionGranted ? 'permission_denied' : null;
        state = mode==='local' ? tr('麦克风：本地') : fault ? tr('麦克风：不可用，{0}', problemText(fault)) : mode==='source' ? tr('麦克风：发送') : tr('麦克风：接收');
        single = mode==='local'?tr('接收其他设备共享的麦克风'):mode==='source'?(muted?tr('取消静音'):tr('静音')):tr('停止接收');
        double = mode==='source'?tr('停止共享'):mode==='receiver'?tr('改为发送'):tr('共享本机麦克风');
    }
    return [state, tr('单击：{0}', single), ...(double ? [tr('双击：{0}', double)] : []), ...(kind==='microphone' && microphoneMode(d)==='receiver' && d.microphone?.waitingForSource ? [tr('当前没有设备共享麦克风')] : [])];
}
function StatusIcon({ ind, action, double, onActivate, tabbable, tooltip, permission, links }: {
    ind: Indicator;
    permission?: { kind: PermissionHintKind; local: boolean; onGrant?: () => void };
    tooltip?: string[];
    links?: Partial<Record<number, ReactNode>>;
    action: string | null;
    double: boolean;
    onActivate?: (mode: 'single' | 'double') => void;
    tabbable: boolean;
}) {
    const gesture = useRef(onActivate);
    const firstDouble = useRef(double);
    const tip = tooltip ? <span className="dev-ico__tip">{tooltip.map((line, index) => <span key={index}>{line}{links?.[index]}</span>)}</span> : ind.tip ?? ind.label;
    const body = (<>
      <Glyph name={ind.glyph} waves={ind.waves} muted={ind.muted}/>
      {ind.badge && <Badge name={ind.badge}/>}
    </>);
    if (permission) return <PermissionHint {...permission} lines={tooltip}>{({ label, onGrant }) => onGrant
        ? <button type="button" className="dev-ico dev-ico--btn ok-focusable" data-tone={ind.tone} data-glyph={ind.glyph} aria-label={label} tabIndex={tabbable ? undefined : -1} onClick={event => { if (event.detail <= 1) onGrant(); }}>{body}</button>
        : <span className="dev-ico" data-tone={ind.tone} data-glyph={ind.glyph} data-disabled={ind.disabled || undefined} role="img" aria-label={label}>{body}</span>}
    </PermissionHint>;
    if (!action || !onActivate) {
        return (<Tooltip interactive={!!links} content={tip}>
        <span className="dev-ico" data-tone={ind.tone} data-glyph={ind.glyph} data-disabled={ind.disabled || undefined} role="img" aria-label={ind.label} aria-disabled={ind.disabled || undefined}>{body}</span>
      </Tooltip>);
    }
    const onClick = (e: MouseEvent<HTMLButtonElement>) => {
        // Whether a double-click exists is decided by the state its first click saw:
        // restoring a disabled input re-renders it as shared before the second click.
        if (e.detail === 2) {
            if (firstDouble.current)
                // Audio double-clicks act on the state seen by their first click,
                // which has already applied its own action optimistically.
                (ind.glyph==='microphone'||ind.glyph==='speaker' ? gesture.current ?? onActivate : onActivate)('double');
        }
        else if (e.detail <= 1) {
            gesture.current = onActivate;
            firstDouble.current = double;
            onActivate('single');
        }
    };
    return (<Tooltip interactive={!!links} content={tip}>
      <button type="button" className="dev-ico dev-ico--btn ok-focusable" data-disabled={ind.disabled || undefined} data-tone={ind.tone} data-glyph={ind.glyph} aria-label={tr('{0}，{1}{2}', ind.label, action, double ? tr('（键盘：Shift + 回车相当于双击）') : '')} tabIndex={tabbable ? undefined : -1} onClick={onClick} onKeyDown={(e) => { if (double && e.key === 'Enter' && e.shiftKey) {
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
    const latency = useCardLatency(d);
    if (d.group === 'self')
        return null;
    const offline = d.group === 'offline';
    return (<span className="dev-conn" role="img" aria-label={offline ? offlineReasonText(d.offlineReason) : latency === null ? tr('延迟未知') : tr('延迟 {0} 毫秒', latency)}>
      <Signal latency={latency} offline={offline}/>
      
      {!offline && (<span className="dev-conn__ms">
          <span className="dev-conn__num">{latency === null ? '—' : Math.round(latency)}</span>{latency !== null && <small>ms</small>}
        </span>)}
    </span>);
}
function MetaLine({ d, now, onToggle, onSpeaker, onMicrophone, onGrantPermission, onSelect, interactive, tabbable, exitName }: {
    d: Device;
    now: number;
    onToggle?: (kind: ShareKind, mode: 'single' | 'double') => void;
    onSpeaker?: (action:SpeakerAction) => void;
    onSelect:()=>void;
    onMicrophone?: (action:MicrophoneAction) => void;
    onGrantPermission?: (kind: 'input_monitoring' | 'microphone') => void;
    interactive: boolean;
    tabbable: boolean;
    exitName: string | null;
}) {
    const microphoneAction = useRef(onMicrophone);
    microphoneAction.current = onMicrophone;
    if (d.group === 'offline') {
        return <span className="dev__meta"><span className="dev__seen">{tr('上次在线 {0}', formatLastSeen(d.lastSeen, now))}</span></span>;
    }
    if (d.paused && d.pauseReason === 'input_backlog') {
        return <span className="dev__meta" title={tr('输入积压超过 5 秒，已暂停共享')}><span className="dev__seen">{tr('输入积压超过 5 秒，已暂停共享')}</span></span>;
    }
    return (<span className="dev__meta">
      <span className="dev__status" onClickCapture={e => { if (interactive && e.detail <= 1) onSelect(); }}
        onKeyDownCapture={e => { if (interactive && e.key === 'Enter' && e.shiftKey) onSelect(); }}>
        {indicators(d, exitName === null).map((ind) => {
            if(ind.glyph==='microphone') {
                const mode=microphoneMode(d);
                const tooltip=audioTooltip(d,'microphone',exitName);
                const link = (label: string, onClick: (()=>void) | undefined) => onClick ? <>{tr('hint-separator|，')}<button type="button" className="permission-hint__action ok-focusable" onClick={onClick}>{label}</button></> : undefined;
                const local=d.group==='self';
                const links = local ? {
                    1: mode==='local' && !d.microphone?.virtualReady ? link(tr('需要安装'), onMicrophone ? ()=>onMicrophone('install') : undefined) : undefined,
                    // A sending microphone's double-click stops sharing, which needs neither setup step.
                    2: mode!=='source' && !d.microphone?.permissionGranted ? link(tr('需要授权'), onGrantPermission ? ()=>onGrantPermission('microphone') : undefined) : undefined,
                } : undefined;
                return <StatusIcon key={ind.glyph} ind={ind} links={links} tooltip={tooltip} tabbable={tabbable} action={interactive?tooltip.slice(1).join('；'):null} double
                    onActivate={onMicrophone?(click)=>microphoneAction.current?.(mode==='source'?(click==='double'?'local':{type:'mute'}):click==='double'?'shared':mode==='local'?'receive':'local'):undefined}/>;
            }
            const kind=ind.glyph as ShareKind;
            const available=kind==='speaker'&&d.speaker.mode!=='unknown'&&d.speaker.mode!=='fault';
            // Selection is handled by the status group, including selection-only audio.
            const activate=kind==='speaker'&&!available?()=>{}:(onToggle||onSpeaker)?(mode:'single'|'double')=>{
                // Exit and forwarding share one gesture set: click mutes, double-click stops sharing.
                if(kind==='speaker'&&(d.speaker.mode==='exit'||d.speaker.mode==='forwarding')) {
                    if(mode==='single')onSpeaker?.({type:'mute'});
                    else onToggle?.(kind,d.speaker.mode==='exit'?'double':'single');
                }
                else onToggle?.(kind,mode);
            }:undefined;
            // A gesture with no action is left out of the tooltip rather than labelled 无操作.
            const single = d.group!=='self' && d.inputPermissionMissing ? null : actionFor(d,kind);
            const tooltip: string[] = kind==='speaker'?audioTooltip(d,'speaker',exitName):[ind.label,...(single ? [tr('单击：{0}', single)] : [])];
            // A disabled input has one gesture: the first click restores sharing and a
            // double-click must not disable it again.
            const inputDouble = (kind === 'keyboard' || kind === 'mouse') && d[kind] !== 'disabled' && canDisableInput(d, kind);
            if (inputDouble) tooltip.push(tr('双击：{0}', tr('input-action|禁用')));
            const needsPermission = (kind === 'mouse' || kind === 'keyboard') && (d.inputPermissionMissing || d.actionability?.[kind] === 'open_permissions' || (d.group === 'self' && d.os === 'mac' && d[kind] === 'fault'));
            const permission = needsPermission ? { kind: d.inputPermissionMissing ? 'input_monitoring' as const : 'permissions' as const, local: d.group === 'self',
                onGrant: !interactive ? undefined : d.inputPermissionMissing && onGrantPermission ? () => onGrantPermission('input_monitoring') : activate ? () => activate('single') : undefined } : undefined;
            return <StatusIcon key={ind.glyph} ind={ind} permission={permission} tooltip={tooltip} tabbable={tabbable} action={interactive && (kind==='speaker' || actionFor(d,kind)) ? tooltip.slice(1).join('；') : null} double={available || inputDouble} onActivate={activate}/>;
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
    const glyph = <svg viewBox="0 0 16 16" aria-hidden><circle cx="8" cy="8" r="5.5"/><path d="m4.1 4.1 7.8 7.8"/></svg>;
    return <PermissionHint kind="accessibility" local={local} onGrant={onGrant}>{({ label, onGrant: grant }) => grant
        ? <button type="button" className="dev__accessibility ok-focusable" aria-label={label} tabIndex={tabbable ? undefined : -1} onClick={grant}>{glyph}</button>
        : <span className="dev__accessibility" role="img" aria-label={label}>{glyph}</span>}
    </PermissionHint>;
}

export interface DeviceListProps {
    devices: Device[];
    selectedId: string | null;
    onSelect: (id: string) => void;
    onToggle?: (id: string, kind: ShareKind, mode: 'single' | 'double') => void;
    onSpeaker?: (id: string, a: SpeakerAction) => void;
    onMicrophone?: (id: string, a: MicrophoneAction) => void;
    onPause?: (id: string, paused: boolean) => void;
    onGrantAccessibility?: () => void;
    onGrantPermission?: (id: string, kind: 'input_monitoring' | 'microphone') => void;
    growIds?: ReadonlySet<string>;
    /** Hidden screens per device id (a device whose sharing is off counts all of its screens). */
    hiddenScreens?: ReadonlyMap<string, number>;
    collapsed: boolean;
    now?: number;
}
export function DeviceList({ devices, selectedId, onSelect, onToggle, onSpeaker, onMicrophone, onPause, onGrantAccessibility, onGrantPermission, growIds, hiddenScreens, collapsed, now = Date.now() }: DeviceListProps) {
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
                  <DeviceItem onGrantPermission={onGrantPermission ? kind => onGrantPermission(d.id, kind) : undefined} onGrantAccessibility={onGrantAccessibility} d={d} now={now} selected={!collapsed && d.id === selectedId} collapsed={collapsed} exitName={exit?.name ?? null} onSelect={() => onSelect(d.id)} onToggle={onToggle ? (kind, mode) => onToggle(d.id, kind, mode) : undefined} onSpeaker={onSpeaker ? (a) => onSpeaker(d.id, a) : undefined} onPause={onPause ? (p) => onPause(d.id, p) : undefined} onMicrophone={onMicrophone ? a=>onMicrophone(d.id,a) : undefined} grow={collapsed && !!growIds?.has(d.id)} hiddenScreens={hiddenScreens?.get(d.id) ?? 0}/>
                </li>))}
            </ul>
          </section>);
        })}
    </div>);
}
function DeviceHit({ d, now, selected, collapsed, inPeek, onSelect }: {
    d: Device; now: number; selected: boolean; collapsed: boolean; inPeek?: boolean; onSelect: () => void;
}) {
    const latency = useCardLatency(d);
    const summary = d.group === 'offline' ? tr('{0}，离线，上次在线 {1}', d.name, formatLastSeen(d.lastSeen, now)) : d.group === 'online' ? tr('{0}，在线，延迟 {1} ms', d.name, latency === null ? tr("未知") : latency) : tr('{0}，本机', d.name);
    const pausedNote = d.group !== 'offline' && d.paused ? tr('，已暂停') : '';
    const screenOff = d.group !== 'offline' && d.screen === 'disabled';
    const game = d.group !== 'offline' && gameActive(d) && (d.cursor === 'local' || d.cursor === 'locked');
    const cursorNote = d.group !== 'offline' && (d.cursor === 'local' || d.cursor === 'locked') ? (d.cursor === 'locked' ? tr('，指针锁定在这里') : tr('，指针在这里')) : '';
    return (<Tooltip interactive={false} placement="right" disabled={!collapsed} content={d.group === 'online' ? `${d.name} · ${latency === null ? tr("延迟未知") : `${Math.round(latency)} ms`}` : d.name}>
      <button type="button" className="dev__hit ok-focusable" aria-current={selected ? 'true' : undefined} aria-disabled={d.group === 'offline' ? 'true' : undefined} aria-label={summary + pausedNote + (screenOff ? tr('，屏幕已禁用') : '') + (game ? tr('，游戏模式已开启') : '') + cursorNote} tabIndex={inPeek ? -1 : undefined} onClick={onSelect}/>
      </Tooltip>);
}
function DeviceSignal({ d, shown }: { d: Device; shown: boolean }) {
    const latency = useCardLatency(d);
    return <SignalMark latency={latency} offline={d.group === 'offline'} shown={shown}/>;
}
export function DeviceItem({ d, now, selected, collapsed, exitName = null, onSelect, onToggle, onSpeaker, onMicrophone, volumeOpen = false, grow = false, inPeek, hiddenScreens = 0, onGrantAccessibility, onGrantPermission }: {
    d: Device;
    now: number;
    selected: boolean;
    collapsed: boolean;
    onSelect: () => void;
    /** The group's audio exit; a local-mode audio icon forwards to it. */
    exitName?: string | null;
    onToggle?: (kind: ShareKind, mode: 'single' | 'double') => void;
    onSpeaker?: (a: SpeakerAction) => void;
    onMicrophone?: (a: MicrophoneAction) => void;
    onPause?: (paused: boolean) => void;
    volumeOpen?: boolean;
    grow?: boolean;
    peek?: PeekHandlers;
    inPeek?: boolean;
    hiddenScreens?: number;
    onGrantAccessibility?: () => void;
    onGrantPermission?: (kind: 'input_monitoring' | 'microphone') => void;
}) {
    selected = selected && !collapsed;
    const game = d.group !== 'offline' && gameActive(d) && (d.cursor === 'local' || d.cursor === 'locked');
    return (<div className="dev" data-ctx={`device:${d.id}`} data-group={d.group} data-current={selected ? '' : undefined} data-grow={grow && hasVolume(d) ? '' : undefined}>
      <DeviceHit d={d} now={now} selected={selected} collapsed={collapsed} inPeek={inPeek} onSelect={onSelect}/>
      <span className="dev__os">
        <OsLogo os={d.os}/>
        
        
        {d.group !== 'self' && <DeviceSignal d={d} shown={collapsed && !(d.group === 'online' && d.paused)}/>}
        {d.group !== 'offline' && <PauseMark shown={!!d.paused}/>}
        
        {game ? <GameMark tooltip={!collapsed}/>
                : d.group !== 'offline' && (d.cursor === 'local' || d.cursor === 'locked') && <CursorMark locked={d.cursor === 'locked'} tooltip={!collapsed}/>}
      </span>
      <span className="dev__text">
        <span className="dev__title"><span className="dev__name">{d.name}</span>{d.accessibilityMissing ? <AccessibilityIndicator local={d.group === 'self'} onGrant={onGrantAccessibility} tabbable={!collapsed && !inPeek}/> : hiddenScreens > 0 && <Tooltip interactive={false} content={tr('{0} 块屏幕已隐藏', hiddenScreens)}><span className="dev__hidden" role="img" aria-label={tr('{0} 块屏幕已隐藏', hiddenScreens)}><svg viewBox="0 0 16 16" aria-hidden><path d="M2 8s2.3-4.2 6-4.2S14 8 14 8s-2.3 4.2-6 4.2S2 8 2 8Z"/><circle cx="8" cy="8" r="1.8"/><path d="M3 3l10 10"/></svg></span></Tooltip>}</span>
        <MetaLine onGrantPermission={onGrantPermission} onSelect={selected ? () => {} : onSelect} onSpeaker={onSpeaker} onMicrophone={onMicrophone} d={d} now={now} onToggle={onToggle} interactive={!collapsed} tabbable={!inPeek} exitName={exitName}/>
      </span>
      <Connection d={d}/>
      
      {onSpeaker && d.group !== 'offline' && <VolumeRow d={d} open={(selected || volumeOpen) && !collapsed} tabbable={!inPeek} onSpeaker={onSpeaker}/>}
      {onMicrophone && d.group !== 'offline' && d.microphone?.mode==='source' && <VolumeRow d={d} microphone open={(selected || volumeOpen) && !collapsed} tabbable={!inPeek} onSpeaker={onMicrophone}/>}
    </div>);
}
