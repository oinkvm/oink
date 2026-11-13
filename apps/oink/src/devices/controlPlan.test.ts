import { describe, expect, it } from 'vitest';
import type { CardAction, DeviceCard } from '../api/cards';
import { controlCommand, controlObservation, refusalFor } from './controlPlan';

describe('refusalFor', () => {
  it('passes a card action\'s own unavailable reason through for every resource (oinkvm/oink#32)', () => {
    const exit = { kind: 'speaker', mode: 'exit' } as const;
    expect(refusalFor({ kind: 'unavailable', reason: 'offline' }, exit)).toBe('offline');
    expect(refusalFor({ kind: 'unavailable', reason: 'busy' }, exit)).toBe('busy');
    expect(refusalFor({ kind: 'unavailable', reason: 'not_member' }, exit)).toBe('not_member');
    expect(refusalFor({ kind: 'unavailable', reason: 'state_unknown' }, exit)).toBe('state_conflict');
    expect(refusalFor({ kind: 'unavailable', reason: 'remote_fault' }, { kind: 'card', resource: 'screen', desired: 'shared' })).toBe('resource_fault');
    expect(refusalFor({ kind: 'unavailable', reason: 'state_stale' }, { kind: 'microphone', mode: 'source' })).toBe('state_conflict');
  });
  it('keeps the per-kind fallback when the card offers no action at all', () => {
    expect(refusalFor(null, { kind: 'speaker', mode: 'exit' })).toBe('unsupported');
    expect(refusalFor(undefined, { kind: 'microphone', mode: 'receiver' })).toBe('state_conflict');
  });
});

describe('microphone modes are one write (global audio configuration)', () => {
  const invoke = (token: string): CardAction => ({ kind: 'invoke', command: 'set_audio_mode', token });
  const card = (mode: 'source' | 'receiver' | 'muted' | 'idle' | 'unavailable', virtualReady = true) => ({
    device: 'a', status: { microphone: { mode, permissionGranted: true, virtualReady, shareAction: invoke('send'), localAction: invoke('local'), receiveAction: invoke('receive') } },
  }) as unknown as DeviceCard;
  it('leaves Send for Local or Receive with the target action itself, never a release first', () => {
    expect(controlCommand(card('source'), { kind: 'microphone', mode: 'muted' })).toEqual(invoke('local'));
    expect(controlCommand(card('source'), { kind: 'microphone', mode: 'receiver' })).toEqual(invoke('receive'));
    expect(controlCommand(card('muted'), { kind: 'microphone', mode: 'source' })).toEqual(invoke('send'));
    expect(controlCommand(card('muted', false), { kind: 'microphone', mode: 'receiver' })).toBeNull();
  });
  it('has no intermediate step', () => {
    const view = { cards: [card('source')] } as unknown as import('../api/cards').DeviceCardsView;
    const observed = controlObservation(view, 'a', { kind: 'microphone', mode: 'muted' });
    expect(observed.intermediate).toBeUndefined();
    expect(observed.microphoneMode).toBe('source');
  });
});
