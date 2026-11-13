import { useEffect, useState } from 'react';
import { api } from '../api/client';
import { agent } from '../api/store';
import type { DeviceCardsView } from '../api/cards';
import type { MobileAudioState } from '../api/types';
import { endpointFault } from './project';

/** Online phones' audio endpoint runtime (support, phase, faults, whether a
 * PC may start them), refreshed on MobileAudioStateChanged. Whether a phone is
 * the exit or the sender is its card mode (global audio configuration); a PC
 * turns it back to local through the card (Clear(phone)), never here. */
export function useMobileAudio(cards: DeviceCardsView | null): ReadonlyMap<string, MobileAudioState> {
    const [states, setStates] = useState<ReadonlyMap<string, MobileAudioState>>(() => new Map());
    const phones = cards?.cards
        .filter(card => !card.isSelf && (card.os === 'android' || card.os === 'ios') && card.connection.kind === 'online')
        .map(card => card.device) ?? [];
    const key = phones.join(',');
    useEffect(() => {
        let live = true;
        const watched = new Set(phones);
        const refresh = (device: string) => {
            void api.mobileAudioState(device).then(
                state => live && setStates(previous => withState(previous, device, state)),
                // An unreadable state means no endpoint action is offered.
                () => live && setStates(previous => withState(previous, device, null)),
            );
        };
        setStates(previous => new Map([...previous].filter(([device]) => watched.has(device))));
        watched.forEach(refresh);
        const unsubscribe = agent.onEvent(event => {
            if (event.type === 'mobile_audio_state_changed' && watched.has(event.data)) refresh(event.data);
        });
        return () => { live = false; unsubscribe(); };
    }, [key]);
    return states;
}

function withState(previous: ReadonlyMap<string, MobileAudioState>, device: string, state: MobileAudioState | null) {
    const next = new Map(previous);
    if (state) next.set(device, state); else next.delete(device);
    return next;
}

export type MobileAudioKind = 'speaker' | 'microphone' | 'forward';
/** A PC starts a phone endpoint only where the agent allows it (an Android
 * phone, privileged mode or not, local for that role). */
export function mobileAudioStart(state: MobileAudioState | undefined, kind: MobileAudioKind) {
    const endpoint = kind === 'speaker' ? state?.speaker_receive : kind === 'forward' ? state?.speaker_send : state?.microphone_send;
    if (!endpoint?.start_allowed) return null;
    return () => api.startMobileAudioEndpoint(endpoint.target);
}

/** protocol v2 EndpointPhase Running. */
const PHASE_RUNNING = 2;

/** How a PC-initiated start settled: still pending (null), started, or the
 * phone's failure or refusal reason (missing microphone permission, busy). */
export function startOutcome(state: MobileAudioState | undefined, kind: MobileAudioKind): 'started' | string | null {
    const endpoint = kind === 'speaker' ? state?.speaker_receive : kind === 'forward' ? state?.speaker_send : state?.microphone_send;
    if (!endpoint) return null;
    const fault = endpointFault(endpoint);
    if (fault) return fault;
    return endpoint.phase === PHASE_RUNNING ? 'started' : null;
}
