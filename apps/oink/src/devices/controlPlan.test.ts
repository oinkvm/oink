import { describe, expect, it } from 'vitest';
import { refusalFor } from './controlPlan';

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
