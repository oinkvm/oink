import type { DeviceCard, DeviceCardsView, ResourceCard } from '../api/cards';
import type { ControlAction } from './controlActions';
export interface CardIntent { device:string; resource:string; desired:string|number|boolean; requestId?:string }
import type { Device } from './types';
import type { PermissionItem } from '../api/types';
import { problemText } from '../i18n';
export function presentCards(view: DeviceCardsView | null, intents: CardIntent[], permissions: PermissionItem[] = []): Device[] {
    const devices = view?.cards.map(card => {
        const s = card.status;
        const icon = (r: ResourceCard<string> | undefined) => r?.icon ?? 'unknown';
        const forwarding = s?.speaker.forwarding;
        const exit = view.exitVolume?.receiverDevice === card.device ? view.exitVolume : null;
        const localOutput = s?.speaker.localOutput ?? null;
        const exitUnknown = s?.speaker.configuredMode==='source' && s.speaker.activity==='unknown';
        const waitingForExit = exitUnknown || s?.speaker.activity==='waiting_for_exit' || (s?.speaker.configuredMode==='source' && s.speaker.activity==='idle' && !s.speaker.receiverDevice);
        const d: Device = {
            id: card.device, name: card.name, os: card.os === 'macos' ? 'mac' : card.os === 'windows' ? 'win' : card.os,
            group: card.isSelf ? 'self' : card.section === 'offline' ? 'offline' : 'online',
            offlineReason:card.connection.kind==='offline'?card.connection.reason:undefined,
            ip: '', port: 0,
            latency: card.connection.kind === 'online' ? card.connection.signal.rttMs : null,
            lastSeen: card.lastSeenMs,
            accessibilityMissing: card.os === 'macos' && (card.isSelf
                ? permissions.some(p => p.kind === 'accessibility' && ['NotGranted', 'NotDetermined', 'Denied'].includes(p.state))
                : s?.accessibilityGranted === false),
            cursor: s?.cursor.icon ?? 'unknown',
            screenSharing: s?.screen.configuredMode == null ? null : s.screen.configuredMode === 'shared',
            screen: s?.screen.icon === 'fault' ? 'fault' : s?.screen.configuredMode === null || !s ? 'unknown' : s.screen.configuredMode === 'local' ? 'disabled' : 'shared',
            mouse: icon(s?.mouse) as Device['mouse'], keyboard: icon(s?.keyboard) as Device['keyboard'],
            clipboard: icon(s?.clipboard) as Device['clipboard'],
            speaker: { activity:s?.speaker.activity, configuredMode:s?.speaker.configuredMode, canMakeExit:s?.speaker.makeExitAction.kind==='invoke', waitingForExit, exitUnknown, selectedExitUnavailable:s?.speaker.activity==='waiting_for_exit' && !!s.speaker.receiverDevice, volumeKnown: exit ? exit.volumePercent !== null : forwarding !== null && forwarding !== undefined, mutedKnown: exit ? exit.muted !== null : forwarding !== null && forwarding !== undefined, canSetVolume: exit ? exit.canSet : !!s?.speaker.forwardingControlsToken, mode: icon(s?.speaker) as Device['speaker']['mode'], volume: exit?.volumePercent ?? forwarding?.volumePercent ?? 0, muted: exit?.muted ?? forwarding?.muted ?? false, localOutput: localOutput && { volume: localOutput.volumePercent, muted: localOutput.muted } },
            microphone: s?.microphone && { mode:s.microphone.mode, permissionGranted:s.microphone.permissionGranted,
                captureActive:s.microphone.captureActive, consumerActive:s.microphone.consumerActive,
                receivingActive:s.microphone.receivingActive, fault:s.microphone.fault,
                virtualReady:s.microphone.virtualReady, waitingForSource:s.microphone.mode==='idle'||s.microphone.sourceDevice===null, volume:s.microphone.forwarding?.volumePercent ?? 100,
                muted:s.microphone.forwarding?.muted ?? false, canSetVolume:!!s.microphone.controlsToken },
            inputPermissionMissing: (card.isSelf && permissions.some(p=>p.kind==='input_monitoring' && ['NotGranted','NotDetermined','Denied'].includes(p.state)))
                || [s?.mouse.fault?.code,s?.keyboard.fault?.code].some(code=>code==='permission_required'||code==='permission_denied'),
            gameMode: s?.game.preference ?? null, gameActive:s?.game.active ?? null, gameDetected: s?.game.detected ?? undefined,
            canDisableMouse: s?.actions.mouseDisabled?.kind === 'invoke',
            canPause: card.pause?.action.kind === 'invoke', actionability: Object.fromEntries((['screen','mouse','keyboard','clipboard','speaker'] as const).map(k=>[k,s?.[k].action.kind??'unavailable'])), paused: card.pause?.paused, pauseReason: card.pause?.reason, unknown: s === null,
            faults: Object.fromEntries((['screen', 'mouse', 'keyboard', 'clipboard', 'speaker'] as const).map(k => [k, problemText(s?.[k].fault?.code ?? 'unrecognized')])),
        };
        for (const p of intents.filter(p => p.device === card.device)) {
            if (p.resource === 'pause')
                d.paused = Boolean(p.desired);
            else if (p.resource === 'game')
                {
                    d.gameMode = p.desired as Device['gameMode'];
                    d.gameActive = p.desired==='on'?true:p.desired==='off'?false:d.gameDetected??null;
                }
            else if (p.resource === 'speaker')
                d.speaker = { ...d.speaker, mode: p.desired as Device['speaker']['mode'], configuredMode:p.desired==='exit'?'receiver':p.desired==='forwarding'?'source':'unused' };
            else if (p.resource === 'volume')
                d.speaker = { ...d.speaker, volume: Number(p.desired) };
            else if (p.resource === 'mute')
                d.speaker = { ...d.speaker, muted: Boolean(p.desired) };
            else if (p.resource === 'screen')
                {
                    if(p.desired==='locked'){d.cursor='locked';continue;}
                    d.screen = p.desired as Device['screen'];
                    d.screenSharing=p.desired==='shared';
                    // A prospective Shared/Disabled choice also releases this
                    // screen's pin; the cursor must not override that choice.
                    if(d.cursor==='locked')d.cursor='local';
                }
            else if (p.resource === 'mouse' || p.resource === 'keyboard' || p.resource === 'clipboard')
                {
                    (d[p.resource] as string) = String(p.desired);
                }
        }
        return d;
    }) ?? [];
    const exit = intents.filter(p=>p.resource==='speaker' && p.desired==='exit').at(-1)?.device;
    if(exit)for(const device of devices)if(device.id!==exit && (device.speaker.configuredMode==='receiver'||device.speaker.mode==='exit')){device.speaker={...device.speaker,mode:'unused',configuredMode:'unused'};}
    return devices;
}
/** Completed-looking intent overlays; confirmed cards remain untouched for rollback. */
export function presentControls(view:DeviceCardsView|null,targets:ReadonlyMap<string,ControlAction[]>,permissions:PermissionItem[]=[]):Device[] {
    const cardIntents=[...targets].flatMap(([device,actions])=>actions.flatMap(action=>action.kind==='card'?[{device,resource:action.resource,desired:action.desired}]:[]));
    const devices=presentCards(view,cardIntents,permissions);
    for(const d of devices)for(const action of targets.get(d.id)??[]) {
        if(action.kind==='speaker') {
            const output=d.speaker.localOutput;
            const becomingExit=action.mode==='exit'&&d.speaker.mode!=='exit';
            d.speaker={...d.speaker,mode:action.mode,configuredMode:action.mode==='exit'?'receiver':action.mode==='forwarding'?'source':'unused',...(becomingExit&&output?{volume:output.volume,muted:output.muted,volumeKnown:true,mutedKnown:true,canSetVolume:true}:{})};
        } else if(action.kind==='speakerVolume') {
            d.speaker={...d.speaker,volume:action.volume,muted:action.muted,
                ...(d.speaker.mode==='exit'?{localOutput:{volume:action.volume,muted:action.muted}}:{})};
        } else if(action.kind==='microphone'&&d.microphone) {
            d.microphone={...d.microphone,mode:action.mode,...(action.mode==='receiver'?{muted:false}:action.mode==='source'?{canSetVolume:true}:{})};
        } else if(action.kind==='microphoneVolume'&&d.microphone) {
            d.microphone={...d.microphone,volume:action.volume,muted:action.muted};
        }
    }
    // A group role has one prospective owner, even while old reports arrive.
    for(const [id,actions] of targets)for(const action of actions) {
        if(action.kind==='speaker'&&action.mode==='exit')for(const d of devices)
            if(d.id!==id&&(d.speaker.configuredMode==='receiver'||d.speaker.mode==='exit'))d.speaker={...d.speaker,mode:'unused',configuredMode:'unused'};
        if(action.kind==='microphone'&&action.mode==='source')for(const d of devices)
            if(d.id!==id&&d.microphone?.mode==='source')d.microphone={...d.microphone,mode:d.microphone.virtualReady?'receiver':'unavailable'};
        if(action.kind==='card'&&action.resource==='screen'&&action.desired==='locked')for(const d of devices)
            if(d.id!==id&&d.cursor==='locked')d.cursor='remote';
    }
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
        case 'microphone': return desired === 'shared' ? s.microphone?.shareAction : s.microphone?.localAction;
        case 'microphone_receive': return s.microphone?.receiveAction;
        case 'game': return s.game.actions[desired as 'auto' | 'on' | 'off'];
        case 'screen': return desired === 'shared' ? a.screenShared : a.screenLocal;
        case 'mouse': return desired === 'disabled' ? a.mouseDisabled : desired === 'shared' ? a.mouseShared : a.mouseLocal;
        // v1 peers can still overlay a saved mode with their guard. Release it
        // first; the controller configures again only if the owner's result differs.
        case 'keyboard': return desired === 'disabled' ? a.keyboardDisabled : s.keyboard.disabled ? a.keyboardEnabled : desired === 'shared' ? a.keyboardShared : a.keyboardLocal;
        case 'clipboard': return desired === 'shared' ? a.clipboardShared : a.clipboardLocal;
        case 'speaker': return desired === 'exit' ? s.speaker.makeExitAction : desired === 'forwarding' ? a.forwardingEnabled : s.speaker.configuredMode === 'receiver' ? s.speaker.action : a.forwardingDisabled;
        default: return null;
    }
}
