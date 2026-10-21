import { tr } from '../i18n';
import { Fragment, useEffect, useRef, useState } from 'react';
import type { KeyboardEvent, MouseEvent, PointerEvent, ReactNode } from 'react';
import { ContextMenu, Icon, MenuCheckboxItem, MenuItem, MenuRadioGroup, MenuRadioItem, MenuSeparator, MenuSub, Segmented, Tooltip } from 'oink-ui';
import { Glyph } from './icons';
import type { GlyphName } from './icons';
import type { Device, GameMode } from './types';
import type { SpeakerAction } from './VolumeRow';
import { faultReason, screenValue } from './data';
import type { ShareKind, ShareValue } from './data';
import type { IncomingRequest, Peer } from '../pairing/data';
type Ctx = {
    kind: 'device' | 'screen' | 'tray' | 'request' | 'nearby';
    id: string;
};
function readCtx(target: EventTarget | null): Ctx | null {
    const el = target instanceof Element ? target.closest<HTMLElement>('[data-ctx]') : null;
    const raw = el?.dataset.ctx;
    if (!raw)
        return null;
    const i = raw.indexOf(':');
    const kind = raw.slice(0, i);
    if (kind !== 'device' && kind !== 'screen' && kind !== 'tray' && kind !== 'request' && kind !== 'nearby')
        return null;
    return { kind, id: raw.slice(i + 1) };
}
type Opt = {
    value: ShareValue | GameMode;
    label: string;
    tip: string;
};
const ROWS: Array<{
    kind: ShareKind | 'game';
    glyph: GlyphName;
    label: string;
    options: Opt[];
}> = [
    { kind: 'screen', glyph: 'screen', get label() {
            return tr('屏幕');
        }, options: [
            { value: 'locked', get label() {
                    return tr('锁定');
                }, get tip() { return tr('指针锁定在该设备'); } },
            { value: 'disabled', get label() {
                    return tr('禁用');
                }, get tip() { return tr('指针不能进入该设备'); } },
            { value: 'shared', get label() {
                    return tr('共享');
                }, get tip() { return tr('指针可以进入该设备'); } },
        ] },
    { kind: 'game', glyph: 'game', get label() {
            return tr('游戏模式');
        }, options: [
            { value: 'auto', get label() {
                    return tr('自动');
                }, get tip() { return tr('自动检测游戏'); } },
            { value: 'on', get label() {
                    return tr('开启');
                }, get tip() { return tr('按 3D 游戏方式发送鼠标事件'); } },
            { value: 'off', get label() {
                    return tr('state|关闭');
                }, get tip() { return tr('按桌面应用方式发送鼠标事件'); } },
        ] },
    { kind: 'mouse', glyph: 'mouse', get label() {
            return tr('鼠标');
        }, options: [
            { value: 'disabled', get label() { return tr('禁用'); }, get tip() { return tr('禁用该设备的鼠标'); } },
            { value: 'local', get label() {
                    return tr('本地');
                }, get tip() { return tr('该设备的鼠标只能控制自己'); } },
            { value: 'shared', get label() {
                    return tr('共享');
                }, get tip() { return tr('该设备的鼠标可以控制其他设备'); } },
        ] },
    { kind: 'keyboard', glyph: 'keyboard', get label() {
            return tr('键盘');
        }, options: [
            { value: 'disabled', get label() {
                    return tr('禁用');
                }, get tip() { return tr('禁用该设备的键盘'); } },
            { value: 'local', get label() {
                    return tr('本地');
                }, get tip() { return tr('该设备的键盘只能控制自己'); } },
            { value: 'shared', get label() {
                    return tr('共享');
                }, get tip() { return tr('该设备的键盘可以控制其他设备'); } },
        ] },
    { kind: 'clipboard', glyph: 'clipboard', get label() {
            return tr('剪贴板');
        }, options: [
            { value: 'local', get label() {
                    return tr('本地');
                }, get tip() { return tr('不共享剪贴板'); } },
            { value: 'shared', get label() {
                    return tr('共享');
                }, get tip() { return tr('共享剪贴板'); } },
        ] },
    { kind: 'speaker', glyph: 'speaker', get label() {
            return tr('音频输出');
        }, options: [
            { value: 'exit', get label() {
                    return tr('出口');
                }, get tip() { return tr('为其他设备播放音频'); } },
            { value: 'unused', get label() {
                    return tr('本地');
                }, get tip() { return tr('在自己的设备上播放音频'); } },
            { value: 'forwarding', get label() {
                    return tr('转发');
                }, get tip() { return tr('在出口设备上播放音频'); } },
        ] },
];
const segOptions = (opts: Opt[]) => opts.map((o) => ({
    value: o.value,
    label: <Tooltip interactive={false} content={o.tip}><span className="devmenu__opt">{o.label}</span></Tooltip>,
    'aria-label': `${o.label}：${o.tip}`,
}));
const valueOf = (d: Device, kind: ShareKind | 'game'): string => (kind === 'game' ? d.gameMode ?? 'unknown' : kind === 'speaker' ? d.speaker.mode : kind === 'screen' ? screenValue(d) : d[kind]);
/** Device glyphs (keyboard, gamepad, speaker) at menu icon size, the same set as the device list. */
const glyph = (name: GlyphName) => <span className="devmenu__glyph"><Glyph name={name} waves={name === 'speaker' ? 3 : 0}/></span>;
const PauseIcon = <Icon><path d="M7.4 5v10M12.6 5v10"/></Icon>;
const PowerIcon = <Icon><path d="M10 3.2v6.3M6.1 5.6a6.2 6.2 0 1 0 7.8 0"/></Icon>;
const LeaveIcon = <Icon><path d="M8.2 3.6H5.4a1.8 1.8 0 0 0-1.8 1.8v9.2a1.8 1.8 0 0 0 1.8 1.8h2.8M12.6 6.6 16 10l-3.4 3.4M16 10H8.4"/></Icon>;
const BlockIcon = <Icon><circle cx="10" cy="10" r="6.6"/><path d="M5.3 5.3l9.4 9.4"/></Icon>;
const UnlockIcon = <Icon><rect x="4.5" y="9" width="11" height="7.5" rx="2"/><path d="M7.2 9V6.6a2.8 2.8 0 0 1 5.5-.7"/></Icon>;
const EyeOffIcon = <Icon><path d="M3 10s2.6-5 7-5 7 5 7 5-2.6 5-7 5-7-5-7-5Z"/><circle cx="10" cy="10" r="2.2"/><path d="M4 4l12 12"/></Icon>;
export interface SideContextMenuProps {
    devices: Device[];
    /** Every screen, hidden ones included: a device's Hide/Show all screens reads them. */
    screens?: { id: string; device: string; name: string; hidden: boolean }[];
    /** Canvas screen context: hide that one screen. */
    onHideScreen: (id: string) => void;
    /** Device menu: hide all of its screens, or show them all again when any is hidden. */
    onScreensHidden: (deviceId: string, hidden: boolean) => void;
    screenEditPending?: boolean;
    requests: IncomingRequest[];
    nearby: Peer[];
    onShare: (id: string, kind: ShareKind, value: ShareValue) => void;
    onGameMode: (id: string, mode: GameMode) => void;
    onPause: (id: string, paused: boolean) => void;
    onLeave: (id: string) => void;
    onEditAddress: (id: string) => void;
    onShowWindow: () => void;
    onAbout: () => void;
    onQuit: () => void;
    onSpeaker: (id: string, a: SpeakerAction) => void;
    onAccept: (req: IncomingRequest) => void;
    onReject: (req: IncomingRequest) => void;
    onPair: (peer: Peer) => void;
    onBlock: (peer: Peer) => void;
    onOpenChange?: (open: boolean, kind: Ctx['kind'] | null) => void;
    children: ReactNode;
    standalone?: boolean;
}
export function SideContextMenu({ devices, screens = [], onHideScreen, onScreensHidden, screenEditPending = false, requests, nearby, onShare, onGameMode, onPause, onLeave, onEditAddress, onShowWindow, onAbout, onQuit, onAccept, onReject, onPair, onBlock, onOpenChange, children, standalone = false }: SideContextMenuProps) {
    const [ctx, setCtx] = useState<Ctx | null>(null);
    const [open, setOpenState] = useState(false);
    const ctxRef = useRef<Ctx | null>(null);
    const setOpen = (o: boolean) => { setOpenState(o); onOpenChange?.(o, o ? ctxRef.current?.kind ?? null : null); };
    const tray = standalone || ctx?.kind === 'tray';
    const screen = ctx?.kind === 'screen' ? screens.find(s => s.id === ctx.id && !s.hidden) : undefined;
    const device = ctx?.kind === 'device' ? devices.find((d) => d.id === ctx.id)
        : tray ? devices.find((d) => d.group === 'self') : undefined;
    const request = ctx?.kind === 'request' ? requests.find((r) => r.id === ctx.id) : undefined;
    const peer = ctx?.kind === 'nearby' ? nearby.find((n) => n.id === ctx.id) : undefined;
    const alive = !!(device || screen || request || peer);
    useEffect(() => { if (open && !alive)
        setOpen(false); }, [open, alive]);
    const lastBody = useRef<ReactNode>(null);
    const panel = !!device;
    const onKeyDownCapture = (e: KeyboardEvent<HTMLDivElement>) => {
        if (!panel)
            return;
        const root = e.currentTarget;
        const active = document.activeElement as HTMLElement | null;
        const back = e.key === 'ArrowUp' || (e.key === 'Tab' && e.shiftKey);
        const fwd = e.key === 'ArrowDown' || (e.key === 'Tab' && !e.shiftKey);
        if (!back && !fwd)
            return;
        const stops = Array.from(root.querySelectorAll<HTMLElement>('[data-stop] [role="radio"][tabindex="0"]:not(:disabled), [data-stop] button:not([role="radio"]):not(:disabled), [data-stop] [role="slider"]:not([aria-disabled="true"])')).filter((x) => !x.closest('[inert]'));
        if (!stops.length)
            return;
        e.preventDefault();
        e.stopPropagation();
        const row = active?.closest('[data-stop]');
        let i = active ? stops.indexOf(active) : -1;
        if (i < 0)
            i = stops.findIndex((s) => s.closest('[data-stop]') === row);
        const next = i < 0 ? (fwd ? 0 : stops.length - 1) : (i + (fwd ? 1 : -1) + stops.length) % stops.length;
        stops[next].focus({ preventScroll: true });
    };
    let body: ReactNode = null;
    if (device) {
        const d = device;
        const ro = d.group === 'offline';
        const self = d.group === 'self';
        const leave = self ? tr('离开设备组') : tr('踢出设备组');
        const locked = d.cursor === 'locked';
        const anyHidden = d.screenSharing === false || d.screen === 'disabled' || screens.some(s => s.device === d.id && s.hidden);
        const exit = devices.find(x => x.group !== 'offline' && x.speaker.mode === 'exit');
        const unavailable = (kind: ShareKind) => d.pending || d.unknown || d.actionability?.[kind] === 'unavailable';
        const leaveItem = <MenuItem icon={LeaveIcon} tone="danger" onSelect={() => onLeave(d.id)}>{leave}</MenuItem>;
        body = <Fragment key={`${tray ? 'tray' : 'device'}:${d.id}`}>
          {ro ? <><MenuItem icon="edit" onSelect={() => onEditAddress(d.id)}>{tr('编辑 IP')}</MenuItem><MenuSeparator/>{leaveItem}</> : <>
            <MenuItem icon={locked ? UnlockIcon : 'lock'} disabled={unavailable('screen')} onSelect={() => onShare(d.id, 'screen', locked ? 'shared' : 'locked')}>{locked ? tr('解锁指针') : tr('锁定指针')}</MenuItem>
            <MenuItem icon={anyHidden ? 'eye' : EyeOffIcon} disabled={unavailable('screen') || screenEditPending} onSelect={() => onScreensHidden(d.id, !anyHidden)}>{anyHidden ? tr('显示全部屏幕') : tr('隐藏全部屏幕')}</MenuItem>
            <MenuSub label={tr('输入设备')} icon={glyph('keyboard')}>
              <div className="devmenu__inputs" onKeyDownCapture={onKeyDownCapture} onKeyDown={e => {if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') e.stopPropagation();}}>
              {ROWS.filter(r => r.kind === 'keyboard' || r.kind === 'mouse').sort((a) => a.kind === 'keyboard' ? -1 : 1).map(r => <div className="devmenu__row" data-stop="" key={r.kind}>
                <span className="devmenu__label" id={`devmenu-${r.kind}-${d.id}`}>{glyph(r.glyph)}{r.label}</span>
                {valueOf(d,r.kind) === 'fault' ? <span className="devmenu__na">{faultReason(d,r.kind as ShareKind)}</span> : <Segmented<string> size="sm" className="devmenu__seg" disabled={unavailable(r.kind as ShareKind)} aria-labelledby={`devmenu-${r.kind}-${d.id}`} options={segOptions(r.options).map(o => ({...o, disabled: r.kind === 'mouse' && o.value === 'disabled' && !d.canDisableMouse}))} value={valueOf(d,r.kind)} onChange={v => onShare(d.id,r.kind as ShareKind,v as ShareValue)}/>}
              </div>)}
              </div>
            </MenuSub>
            <MenuSub label={tr('游戏模式')} icon={glyph('game')}>
              <MenuRadioGroup aria-label={tr('游戏模式')} value={d.gameMode ?? ''} onChange={v => onGameMode(d.id,v as GameMode)}>
                {ROWS.find(r => r.kind === 'game')!.options.map(o => <MenuRadioItem key={o.value} value={o.value} closeOnSelect disabled={d.pending || d.unknown}>{o.label}</MenuRadioItem>)}
              </MenuRadioGroup>
            </MenuSub>
            <MenuItem icon={glyph('speaker')} disabled={d.pending || (unavailable('speaker') && !d.speaker.canMakeExit)} onSelect={() => onShare(d.id,'speaker',d.speaker.mode === 'exit' ? 'unused' : 'exit')}>{d.speaker.mode === 'exit' ? tr('关闭音频出口') : tr('设为音频出口')}</MenuItem>
            {exit && exit.id !== d.id && <MenuCheckboxItem closeOnSelect disabled={unavailable('speaker')} checked={d.speaker.mode === 'forwarding'} onCheckedChange={on => onShare(d.id,'speaker',on ? 'forwarding' : 'unused')}>{tr('转发音频到 {0}',exit.name)}</MenuCheckboxItem>}
            <MenuSeparator/>
            <MenuItem icon={d.paused ? 'play' : PauseIcon} disabled={d.pending || d.canPause === false} onSelect={() => onPause(d.id,!d.paused)}>{d.paused ? tr('恢复共享') : tr('暂停共享')}</MenuItem>
            {leaveItem}
          </>}
          {tray && <><MenuSeparator/><MenuItem icon="external" onSelect={onShowWindow}>{tr('显示界面')}</MenuItem><MenuItem icon="info" onSelect={onAbout}>{tr('关于 Oink')}</MenuItem><MenuItem icon={PowerIcon} onSelect={onQuit}>{tr('退出 Oink')}</MenuItem></>}
        </Fragment>;
    }
    else if (screen) {
        body = <Fragment key={`screen:${screen.id}`}>
          <MenuItem icon={EyeOffIcon} disabled={screenEditPending} onSelect={() => onHideScreen(screen.id)}>{tr('隐藏此屏幕')}</MenuItem>
        </Fragment>;
    }
    else if (request) {
        const req = request;
        body = (<Fragment key={`request:${req.id}`}>
        <MenuItem icon="check" onSelect={() => onAccept(req)}>{tr('接受')}</MenuItem>
        <MenuItem icon="x" onSelect={() => onReject(req)}>{tr('拒绝')}</MenuItem>
        <MenuSeparator />
        <MenuItem icon={BlockIcon} tone="danger" onSelect={() => onBlock(req)}>{tr('屏蔽')}</MenuItem>
      </Fragment>);
    }
    else if (peer) {
        const p = peer;
        body = (<Fragment key={`nearby:${p.id}`}>
        <MenuItem icon="plus" disabled={!p.compatible} onSelect={() => onPair(p)}>{tr('配对')}</MenuItem>
        <MenuSeparator />
        <MenuItem icon={BlockIcon} tone="danger" onSelect={() => onBlock(p)}>{tr('屏蔽')}</MenuItem>
      </Fragment>);
    }
    if (body)
        lastBody.current = body;
    const pick = (e: MouseEvent<HTMLElement>) => {
        const c = readCtx(e.target);
        if (!c) {
            e.preventDefault();
            return;
        }
        ctxRef.current = c;
        setCtx(c);
    };
    const pickTouch = (e: PointerEvent<HTMLElement>) => {
        if (e.pointerType !== 'touch')
            return;
        const c = readCtx(e.target);
        if (!c) {
            e.preventDefault();
            return;
        }
        ctxRef.current = c;
        setCtx(c);
    };
    if (standalone)
        return <div className="devmenu" onKeyDownCapture={onKeyDownCapture}>{body}</div>;
    return (<ContextMenu open={open} onOpenChange={setOpen} menu={lastBody.current} className="devmenu">
      <div className="side-ctx" onContextMenu={pick} onPointerDown={pickTouch}>{children}</div>
    </ContextMenu>);
}
