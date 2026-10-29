import { tr } from '../i18n';
import type { Device, ScreenState } from './types';
export type ShareKind = 'screen' | 'mouse' | 'keyboard' | 'clipboard' | 'speaker';
/** Match the input menu's capability and permission gates. */
export function canDisableInput(d: Device, kind: 'keyboard' | 'mouse'): boolean {
    return !d.unknown && !d.inputPermissionMissing && d.group !== 'offline'
        && d[kind] !== 'fault' && d[kind] !== 'unknown'
        && (!d.actionability?.[kind] || d.actionability[kind] === 'invoke')
        && (kind === 'keyboard' || d.canDisableMouse === true);
}
export function toggleShare(d: Device, kind: ShareKind): Device | null {
    switch (kind) {
        case 'screen':
            return null;
        case 'mouse':
        case 'clipboard': {
            const v = d[kind];
            if (v === 'fault' || v === 'unknown')
                return null;
            return { ...d, [kind]: v === 'shared' ? 'local' : 'shared' };
        }
        case 'keyboard': {
            const v = d.keyboard;
            if (v === 'fault' || v === 'unknown')
                return null;
            if (v === 'disabled')
                return { ...d, keyboard: 'shared' };
            return { ...d, keyboard: v === 'shared' ? 'local' : 'shared' };
        }
        case 'speaker': {
            const sp = d.speaker;
            if (sp.mode === 'fault' || sp.mode === 'unknown')
                return null;
            if (sp.mode === 'exit')
                return leaveExit(d);
            return sp.mode === 'unused'
                ? { ...d, speaker: { ...sp, mode: 'forwarding', volume: sp.volume } }
                : { ...d, speaker: { ...sp, mode: 'unused' } };
        }
    }
}
export function faultReason(d: Device, kind: ShareKind): string {
    return d.faults?.[kind as keyof NonNullable<Device['faults']>] ?? tr('状态未知');
}
export const hasThirdState = (kind: ShareKind) => kind === 'keyboard' || kind === 'speaker';
const leaveExit = (d: Device): Device => ({ ...d, speaker: { ...d.speaker, mode: 'unused' } });
export function toggleThird(devices: Device[], id: string, kind: ShareKind): Device[] | null {
    const d = devices.find((x) => x.id === id);
    if (!d || !hasThirdState(kind))
        return null;
    if (kind === 'keyboard') {
        const v = d.keyboard;
        if (v === 'fault' || v === 'unknown')
            return null;
        const next: Device = v === 'disabled'
            ? { ...d, keyboard: 'shared' }
            : { ...d, keyboard: 'disabled' };
        return devices.map((x) => (x.id === id ? next : x));
    }
    const mode = d.speaker.mode;
    if (mode === 'fault')
        return null;
    if (mode === 'exit')
        return devices.map((x) => (x.id === id ? leaveExit(x) : x));
    return devices.map((x) => {
        if (x.id === id)
            return { ...x, speaker: { ...x.speaker, mode: 'exit', volume: x.speaker.volume } };
        return x.speaker.mode === 'exit' ? leaveExit(x) : x;
    });
}
export type ShareValue = 'local' | 'shared' | 'disabled' | 'locked' | 'unused' | 'forwarding' | 'exit';
export const screenValue = (d: Device): 'locked' | ScreenState => (d.cursor === 'locked' ? 'locked' : d.screen);
export const gameActive = (d: Device) => d.gameActive === true;
const MIN = 60_000, HOUR = 60 * MIN, DAY = 24 * HOUR;
export function formatLastSeen(t: number | null, now = Date.now()): string {
    if (t === null)
        return tr('未知');
    const diff = Math.max(0, now - t);
    if (diff < MIN)
        return tr('刚刚');
    if (diff < HOUR)
        return tr('{0} 分钟前', Math.floor(diff / MIN));
    if (diff < DAY)
        return tr('{0} 小时前', Math.floor(diff / HOUR));
    return tr('{0} 天前', Math.floor(diff / DAY));
}

export function offlineReasonText(reason:Device['offlineReason']):string {
  switch(reason){case 'connecting':return tr('正在连接');case 'address_stale':return tr('设备地址已失效');case 'protocol_incompatible':return tr('协议版本不兼容');default:return tr('设备已离线');}
}
