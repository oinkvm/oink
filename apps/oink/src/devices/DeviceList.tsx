import { tr } from '../i18n';
import type { MouseEvent } from 'react';
import { Button, Icon, Tooltip } from 'oink-ui';
import { Badge, CursorMark, GameMark, Glyph, OsLogo, PauseMark, ScreenOffMark, Signal, SignalMark, Slash } from './icons';
import type { BadgeName, GlyphName } from './icons';
import type { Device, DeviceGroup } from './types';
import { faultReason, formatLastSeen, offlineReasonText, gameActive, hasThirdState } from './data';
import type { ShareKind } from './data';
import { VolumeRow } from './VolumeRow';
import type { SpeakerAction } from './VolumeRow';
import { usePeekScope } from './peek';
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
}
export const volumeWaves = (v: number) => (v <= 0 ? 0 : v <= 33 ? 1 : v <= 66 ? 2 : v <= 100 ? 3 : 4);
export function indicators(d: Device, noExit = false): Indicator[] {
    const fault = (glyph: GlyphName, name: string, kind: ShareKind): Indicator => ({ glyph, tone: 'warn', badge: 'warning', label: tr('{0}不可用：{1}', name, faultReason(d, kind)) });
    const share = (glyph: GlyphName, name: string, s: 'shared' | 'local' | 'fault' | 'unknown'): Indicator => (s === 'shared' ? { glyph, tone: 'good', badge: 'wifi', label: tr('{0}：共享', name) }
        : s === 'local' ? { glyph, tone: 'muted', label: tr('{0}：本地', name) }
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
    const speaker: Indicator = sp.mode === 'unknown' ? { glyph: 'speaker', tone: 'warn', badge: 'warning', label: tr('音频输出：不可用') } : sp.mode === 'unused' ? { glyph: 'speaker', tone: 'muted', get label() {
            return tr('音频输出：本地');
        } }
        : sp.mode === 'exit' ? { glyph: 'speaker', tone: 'accent', badge: 'receive', ...vol(sp.muted ? tr('出口静音') : tr('作为出口')) }
            : sp.mode === 'fault' ? fault('speaker', tr('音频输出'), 'speaker')
                : (sp.waitingForExit ?? noExit) ? { glyph: 'speaker', tone: 'good', badge: 'warning', ...vol(sp.selectedExitUnavailable ? tr('所选出口不可用') : tr('无出口')) }
                    : { glyph: 'speaker', tone: 'good', badge: 'wifi', ...vol(sp.muted ? tr('转发静音') : tr('正在转发')) };
    return [share('mouse', tr('鼠标'), d.mouse), keyboard, share('clipboard', tr('剪贴板'), d.clipboard), speaker];
}
function actionFor(d: Device, kind: ShareKind): string | null {
    if (kind === 'speaker' && d.speaker.mode === 'unknown' && d.speaker.canMakeExit) return tr('双击设为出口');
    if (d.actionability?.[kind] === 'open_permissions') return tr('点击前往设置 › 权限');
    if (d.actionability?.[kind] === 'unavailable') return null;
    if (d.unknown || (kind === 'speaker' ? d.speaker.mode === 'unknown' : d[kind] === 'unknown'))
        return null;
    const fault = kind === 'speaker' ? d.speaker.mode === 'fault' : d[kind] === 'fault';
    if (fault)
        return d.group === 'self' && d.os === 'mac' && (kind === 'mouse' || kind === 'keyboard') ? tr('点击前往设置 › 权限') : null;
    switch (kind) {
        case 'keyboard':
            if (d.keyboard === 'disabled')
                return tr('点击解除禁用，回到{0}', d.keyboardBeforeLock === 'local' ? tr('本地') : tr('共享'));
            return tr('点击切换为{0}，双击禁用', d.keyboard === 'shared' ? tr('本地') : tr('共享'));
        case 'speaker':
            if (d.speaker.mode === 'exit')
                return tr('点击取消出口，回到本地');
            return tr('点击切换为{0}，双击设为接收', d.speaker.mode === 'unused' ? tr('共享') : tr('本地'));
        default:
            return tr('点击切换为{0}', d[kind] === 'shared' ? tr('本地') : tr('共享'));
    }
}
export const hasExit = (devices: Device[]) => devices.some((d) => d.group !== 'offline' && d.speaker.mode === 'exit');
const hasVolume = (d: Device) => d.speaker.mode === 'forwarding' || d.speaker.mode === 'exit';
function StatusIcon({ ind, action, double, onActivate, tabbable }: {
    ind: Indicator;
    action: string | null;
    double: boolean;
    onActivate?: (mode: 'single' | 'double') => void;
    tabbable: boolean;
}) {
    const body = (<>
      <Glyph name={ind.glyph} waves={ind.waves}/>
      {ind.badge && <Badge name={ind.badge}/>}
      {ind.muted && <Slash />}
    </>);
    if (!action || !onActivate) {
        return (<Tooltip interactive={false} content={ind.tip ?? ind.label}>
        <span className="dev-ico" data-tone={ind.tone} role="img" aria-label={ind.label}>{body}</span>
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
function MetaLine({ d, now, onToggle, interactive, tabbable, noExit }: {
    d: Device;
    now: number;
    onToggle?: (kind: ShareKind, mode: 'single' | 'double') => void;
    interactive: boolean;
    tabbable: boolean;
    noExit: boolean;
}) {
    if (d.group === 'offline') {
        return <span className="dev__meta"><span className="dev__seen">{tr('上次在线 {0}', formatLastSeen(d.lastSeen, now))}</span></span>;
    }
    if (d.paused && d.pauseReason === 'input_backlog') {
        return <span className="dev__meta" title={tr('输入积压超过 5 秒，已暂停共享')}><span className="dev__seen">{tr('输入积压超过 5 秒，已暂停共享')}</span></span>;
    }
    return (<span className="dev__meta">
      <span className="dev__status">
        {indicators(d, noExit).map((ind) => {
            const kind = ind.glyph as ShareKind;
            return (<StatusIcon key={ind.glyph} ind={ind} tabbable={tabbable} action={interactive ? actionFor(d, kind) : null} double={hasThirdState(kind)} onActivate={onToggle ? (mode) => { if (kind === 'speaker' && d.speaker.mode === 'unknown' && mode === 'single') return; onToggle(kind, mode); } : undefined}/>);
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
export interface DeviceListProps {
    devices: Device[];
    selectedId: string | null;
    onSelect: (id: string) => void;
    onToggle?: (id: string, kind: ShareKind, mode: 'single' | 'double') => void;
    onSpeaker?: (id: string, a: SpeakerAction) => void;
    onPause?: (id: string, paused: boolean) => void;
    growIds?: ReadonlySet<string>;
    collapsed: boolean;
    now?: number;
}
export function DeviceList({ devices, selectedId, onSelect, onToggle, onSpeaker, onPause, growIds, collapsed, now = Date.now() }: DeviceListProps) {
    const noExit = !hasExit(devices);
    const peek = usePeekScope();
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
                  <DeviceItem d={d} now={now} selected={d.id === selectedId} collapsed={collapsed} noExit={noExit} onSelect={() => onSelect(d.id)} onToggle={onToggle ? (kind, mode) => onToggle(d.id, kind, mode) : undefined} onSpeaker={onSpeaker ? (a) => onSpeaker(d.id, a) : undefined} onPause={onPause ? (p) => onPause(d.id, p) : undefined} volumeOpen={!collapsed && d.id === selectedId} grow={collapsed && !!growIds?.has(d.id)} peek={collapsed && peek ? peek.handlers(d.id) : undefined}/>
                </li>))}
            </ul>
          </section>);
        })}
    </div>);
}
export function DeviceItem({ d, now, selected, collapsed, noExit = false, onSelect, onToggle, onSpeaker, onPause, volumeOpen = false, grow = false, peek, inPeek }: {
    d: Device;
    now: number;
    selected: boolean;
    collapsed: boolean;
    onSelect: () => void;
    noExit?: boolean;
    onToggle?: (kind: ShareKind, mode: 'single' | 'double') => void;
    onSpeaker?: (a: SpeakerAction) => void;
    onPause?: (paused: boolean) => void;
    volumeOpen?: boolean;
    grow?: boolean;
    peek?: PeekHandlers;
    inPeek?: boolean;
}) {
    const summary = d.group === 'offline' ? tr('{0}，离线，上次在线 {1}', d.name, formatLastSeen(d.lastSeen, now)) : d.group === 'online' ? tr('{0}，在线，延迟 {1} ms', d.name, d.latency === null ? tr("未知") : d.latency) : tr('{0}，本机', d.name);
    const pausedNote = d.group !== 'offline' && d.paused ? tr('，已暂停') : '';
    const screenOff = d.group !== 'offline' && d.screen === 'disabled';
    const game = d.group !== 'offline' && gameActive(d);
    const cursorNote = d.group !== 'offline' && (d.cursor === 'local' || d.cursor === 'locked') ? (d.cursor === 'locked' ? tr('，指针锁定在这里') : tr('，指针在这里')) : '';
    return (<div aria-busy={d.pending || undefined} className="dev" data-ctx={`device:${d.id}`} data-group={d.group} data-current={selected ? '' : undefined} data-grow={grow && hasVolume(d) ? '' : undefined}>
      <button type="button" className="dev__hit ok-focusable" aria-current={selected ? 'true' : undefined} aria-disabled={d.group === 'offline' ? 'true' : undefined} aria-label={summary + pausedNote + (screenOff ? tr('，屏幕已禁用') : '') + (game ? tr('，游戏模式已开启') : '') + cursorNote} tabIndex={inPeek ? -1 : undefined} onClick={onSelect} {...peek}/>
      <span className="dev__os">
        <OsLogo os={d.os}/>
        
        
        {d.group !== 'self' && <SignalMark latency={d.latency} offline={d.group === 'offline'} shown={collapsed && !(d.group === 'online' && d.paused)}/>}
        {d.group !== 'offline' && <PauseMark shown={!!d.paused}/>}
        
        {screenOff
            ? <ScreenOffMark tooltip={!collapsed}/>
            : game ? <GameMark tooltip={!collapsed}/>
                : d.group !== 'offline' && (d.cursor === 'local' || d.cursor === 'locked') && <CursorMark locked={d.cursor === 'locked'} tooltip={!collapsed}/>}
      </span>
      <span className="dev__text">
        <span className="dev__name">{d.name}</span>
        <MetaLine d={d} now={now} onToggle={onToggle} interactive={!collapsed} tabbable={!inPeek} noExit={noExit}/>
      </span>
      <Connection d={d}/>
      
      {d.group === 'self' && onPause && (<Tooltip interactive={false} content={d.paused ? tr('恢复共享') : tr('暂停共享')}>
          <Button variant="ghost" size="sm" iconOnly className="dev__pause" disabled={d.canPause === false || d.pending} aria-label={d.paused ? tr('恢复共享') : tr('暂停共享')} tabIndex={inPeek ? -1 : undefined} onClick={() => onPause(!d.paused)}>
            {d.paused ? <Icon name="play"/> : <Icon><path d="M7.4 5v10M12.6 5v10"/></Icon>}
          </Button>
        </Tooltip>)}
      {onSpeaker && d.group !== 'offline' && <VolumeRow d={d} open={volumeOpen && !collapsed} tabbable={!inPeek} onSpeaker={onSpeaker}/>}
    </div>);
}
