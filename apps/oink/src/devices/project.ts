import type { DeviceCard, DeviceCardsView, ResourceCard } from '../api/cards';
import type { PendingControl } from '../api/store';
import type { Device } from './types';
import { problemText } from '../i18n';
export function presentCards(view: DeviceCardsView | null, pending: PendingControl[]): Device[] {
    const devices = view?.cards.map(card => {
        const s = card.status;
        const icon = (r: ResourceCard<string> | undefined) => r?.icon ?? 'unknown';
        const forwarding = s?.speaker.forwarding;
        const exit = view.exitVolume?.receiverDevice === card.device ? view.exitVolume : null;
        const d: Device = {
            id: card.device, name: card.name, os: card.os === 'macos' ? 'mac' : card.os === 'windows' ? 'win' : card.os,
            group: card.isSelf ? 'self' : card.section === 'offline' ? 'offline' : 'online',
            offlineReason:card.connection.kind==='offline'?card.connection.reason:undefined,
            ip: '', port: 0,
            latency: card.connection.kind === 'online' ? card.connection.signal.rttMs : null,
            lastSeen: card.lastSeenMs,
            cursor: s?.cursor.icon ?? 'unknown',
            screenSharing: s?.screen.configuredMode == null ? null : s.screen.configuredMode === 'shared',
            screen: s?.screen.icon === 'fault' ? 'fault' : s?.screen.configuredMode === null || !s ? 'unknown' : s.screen.configuredMode === 'local' ? 'disabled' : 'shared',
            mouse: icon(s?.mouse) as Device['mouse'], keyboard: icon(s?.keyboard) as Device['keyboard'], keyboardBeforeLock: s?.keyboard.disabled ? s.keyboard.configuredMode ?? undefined : undefined,
            clipboard: icon(s?.clipboard) as Device['clipboard'],
            speaker: { canMakeExit:s?.speaker.makeExitAction.kind==='invoke', waitingForExit:s?.speaker.activity==='waiting_for_exit' || (s?.speaker.configuredMode==='source' && s.speaker.activity==='idle' && !s.speaker.receiverDevice), selectedExitUnavailable:s?.speaker.activity==='waiting_for_exit' && !!s.speaker.receiverDevice, volumeKnown: exit ? exit.volumePercent !== null : forwarding !== null && forwarding !== undefined, mutedKnown: exit ? exit.muted !== null : forwarding !== null && forwarding !== undefined, canSetVolume: exit ? exit.canSet : !!s?.speaker.forwardingControlsToken, mode: icon(s?.speaker) as Device['speaker']['mode'], volume: exit?.volumePercent ?? forwarding?.volumePercent ?? 0, muted: exit?.muted ?? forwarding?.muted ?? false },
            gameMode: s?.game.preference ?? null, gameActive:s?.game.active ?? null, gameDetected: s?.game.detected ?? undefined,
            canPause: card.pause?.action.kind === 'invoke', actionability: Object.fromEntries((['screen','mouse','keyboard','clipboard','speaker'] as const).map(k=>[k,s?.[k].action.kind??'unavailable'])), paused: card.pause?.paused, pauseReason: card.pause?.reason, unknown: s === null,
            faults: Object.fromEntries((['screen', 'mouse', 'keyboard', 'clipboard', 'speaker'] as const).map(k => [k, problemText(s?.[k].fault?.code ?? 'unrecognized')])),
        };
        for (const p of pending.filter(p => p.device === card.device)) {
            d.pending = true;
            if (p.resource === 'pause')
                d.paused = Boolean(p.desired);
            else if (p.resource === 'game')
                d.gameMode = p.desired as Device['gameMode'];
            else if (p.resource === 'speaker')
                d.speaker = { ...d.speaker, mode: p.desired as Device['speaker']['mode'] };
            else if (p.resource === 'speaker_controls') d.speaker = typeof p.desired === 'boolean' ? {...d.speaker, muted:p.desired} : {...d.speaker,volume:Number(p.desired),muted:false};
            else if (p.resource === 'volume')
                d.speaker = { ...d.speaker, volume: Number(p.desired) };
            else if (p.resource === 'mute')
                d.speaker = { ...d.speaker, muted: Boolean(p.desired) };
            else if (p.resource === 'screen')
                {
                    d.screen = p.desired as Device['screen'];
                    d.screenSharing=p.desired==='shared';
                    // A prospective Shared/Disabled choice also releases this
                    // screen's pin; the cursor must not override that choice.
                    if(d.cursor==='locked')d.cursor='local';
                }
            else if (p.resource === 'mouse' || p.resource === 'keyboard' || p.resource === 'clipboard')
                (d[p.resource] as string) = String(p.desired);
        }
        return d;
    }) ?? [];
    const exit = pending.filter(p=>p.resource==='speaker' && p.desired==='exit').at(-1)?.device;
    if(exit)for(const device of devices)if(device.id!==exit && device.speaker.mode==='exit'){device.speaker={...device.speaker,mode:'unused'};device.pending=true;}
    return devices;
}
export function resourceAction(card: DeviceCard, resource: string, desired: string | number | boolean) {
    const s = card.status;
    if (resource === 'pause')
        return card.pause?.action ?? null;
    if (!s)
        return null;
    const a = s.actions;
    switch (resource) {
        case 'game': return s.game.actions[desired as 'auto' | 'on' | 'off'];
        case 'screen': return desired === 'shared' ? a.screenShared : a.screenLocal;
        case 'mouse': return desired === 'shared' ? a.mouseShared : a.mouseLocal;
        case 'keyboard': return desired === 'disabled' ? a.keyboardDisabled : s.keyboard.disabled ? a.keyboardEnabled : desired === 'shared' ? a.keyboardShared : a.keyboardLocal;
        case 'clipboard': return desired === 'shared' ? a.clipboardShared : a.clipboardLocal;
        case 'speaker': return desired === 'exit' ? s.speaker.makeExitAction : desired === 'forwarding' ? a.forwardingEnabled : s.speaker.icon === 'exit' ? s.speaker.action : a.forwardingDisabled;
        default: return null;
    }
}
