import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import type { DeviceControlReply, DeviceControlResult } from '../api/cards';
import { SEND_INTERVAL_MS, SpeakerControls } from './speakerControls';
import type { SpeakerFacts, SpeakerTarget } from './speakerControls';

beforeEach(() => { vi.useFakeTimers(); });
afterEach(() => { vi.useRealTimers(); });

const completed = (result: DeviceControlResult): DeviceControlReply => ({ type: 'device_control_completed', data: { requestId: 'x', result } });
const applied = completed({ outcome: 'applied' });

/** One owner with a revision fence; `bump` publishes the next card snapshot. */
function owner(start: SpeakerTarget = { volume: 50, muted: false }) {
  let revision = 1;
  let facts: SpeakerFacts = { ...start, token: 't1', exit: false };
  const sent: { id: string; token: string; target: SpeakerTarget }[] = [];
  const targets: (number | undefined)[] = [];
  const failures: string[] = [];
  let reply: (id: string, target: SpeakerTarget) => Promise<DeviceControlReply> = async () => applied;
  let n = 0;
  const controls = new SpeakerControls({
    read: () => facts,
    send: (id, f, target) => { sent.push({ id, token: f.token, target }); return reply(id, target); },
    onTargets: map => targets.push(map.get('peer')?.volume),
    onFailure: reason => failures.push(reason),
    newId: () => `r${++n}`,
  });
  const bump = (target: SpeakerTarget) => { facts = { ...target, token: `t${++revision}`, exit: false }; controls.refresh(); };
  return { controls, sent, targets, failures, bump, facts: () => facts, setReply: (fn: typeof reply) => { reply = fn; } };
}

it('bounds a rapid drag against a fast local owner and lands the release value last', async () => {
  const o = owner();
  // A local owner answers at once and publishes its new revision right after.
  o.setReply(async (_id, target) => { queueMicrotask(() => o.bump(target)); return applied; });
  for (let i = 0; i < 100; i++) {
    o.controls.set('peer', { volume: 51 + (i % 50), muted: false });
    await vi.advanceTimersByTimeAsync(8);
  }
  o.controls.set('peer', { volume: 137, muted: false }, true);
  await vi.advanceTimersByTimeAsync(200);
  expect(o.sent.length).toBeLessThanOrEqual(Math.ceil(800 / SEND_INTERVAL_MS) + 2);
  expect(o.sent.length).toBeGreaterThan(5);
  expect(o.sent.at(-1)!.target.volume).toBe(137);
  expect(o.facts().volume).toBe(137);
  // Every send used a fresh version token: no request was refused as stale.
  expect(new Set(o.sent.map(s => s.token)).size).toBe(o.sent.length);
  expect(o.targets.at(-1)).toBeUndefined();
});

it('keeps one request in flight and sends only the newest position after the new version arrives', async () => {
  const o = owner();
  const pending: ((reply: DeviceControlReply) => void)[] = [];
  o.setReply(() => new Promise(resolve => pending.push(resolve)));
  o.controls.set('peer', { volume: 10, muted: false });
  o.controls.set('peer', { volume: 20, muted: false });
  o.controls.set('peer', { volume: 30, muted: false });
  await vi.advanceTimersByTimeAsync(100);
  expect(o.sent.map(s => s.target.volume)).toEqual([10]);
  expect(o.targets.at(-1)).toBe(30);
  pending[0](applied);
  await vi.advanceTimersByTimeAsync(100);
  // The spent token would be refused as stale_revision: wait for the next card snapshot.
  expect(o.sent).toHaveLength(1);
  // That snapshot still shows 10 (an older value); the slider keeps showing 30.
  o.bump({ volume: 10, muted: false });
  expect(o.targets.at(-1)).toBe(30);
  await vi.advanceTimersByTimeAsync(0);
  expect(o.sent.map(s => [s.target.volume, s.token])).toEqual([[10, 't1'], [30, 't2']]);
  pending[1](applied);
  o.bump({ volume: 30, muted: false });
  await vi.advanceTimersByTimeAsync(100);
  expect(o.sent).toHaveLength(2);
  expect(o.targets.at(-1)).toBeUndefined();
});

it('sends the release value immediately instead of waiting out the send spacing', async () => {
  const o = owner();
  o.setReply(async (_id, target) => { queueMicrotask(() => o.bump(target)); return applied; });
  o.controls.set('peer', { volume: 60, muted: false });
  await vi.advanceTimersByTimeAsync(1);
  o.controls.set('peer', { volume: 61, muted: false });
  await vi.advanceTimersByTimeAsync(1);
  expect(o.sent.map(s => s.target.volume)).toEqual([60]);
  o.controls.set('peer', { volume: 62, muted: false }, true);
  await vi.advanceTimersByTimeAsync(0);
  expect(o.sent.map(s => s.target.volume)).toEqual([60, 62]);
  await vi.advanceTimersByTimeAsync(200);
  expect(o.sent).toHaveLength(2);
});

it('ignores a late or foreign completion so an older request cannot regress the value', async () => {
  const o = owner();
  const pending: ((reply: DeviceControlReply) => void)[] = [];
  o.setReply(() => new Promise(resolve => pending.push(resolve)));
  o.controls.set('peer', { volume: 70, muted: false });
  await vi.advanceTimersByTimeAsync(0);
  // Remote owners reply "accepted"; the completion arrives later as an event.
  pending[0]({ type: 'device_control_accepted', data: { requestId: 'r1' } });
  o.controls.set('peer', { volume: 90, muted: false });
  o.controls.complete('r0', { outcome: 'applied' });
  await vi.advanceTimersByTimeAsync(100);
  expect(o.sent).toHaveLength(1);
  expect(o.targets.at(-1)).toBe(90);
  o.controls.complete('r1', { outcome: 'applied' });
  o.bump({ volume: 70, muted: false });
  await vi.advanceTimersByTimeAsync(100);
  expect(o.sent.map(s => s.target.volume)).toEqual([70, 90]);
  // A duplicate completion of the first request changes nothing.
  o.controls.complete('r1', { outcome: 'applied' });
  expect(o.targets.at(-1)).toBe(90);
});

it('retries the newest position after a stale refusal and reports other failures once', async () => {
  const o = owner();
  o.setReply(async () => completed({ outcome: 'refused', reason: 'stale_revision', unknownCode: null }));
  o.controls.set('peer', { volume: 40, muted: false }, true);
  await vi.advanceTimersByTimeAsync(100);
  expect(o.sent).toHaveLength(1);
  expect(o.failures).toEqual([]);
  o.setReply(async (_id, target) => { queueMicrotask(() => o.bump(target)); return applied; });
  o.bump({ volume: 33, muted: false }); // a volume key moved it meanwhile
  await vi.advanceTimersByTimeAsync(100);
  expect(o.sent.map(s => [s.target.volume, s.token])).toEqual([[40, 't1'], [40, 't2']]);
  expect(o.facts().volume).toBe(40);

  o.setReply(async () => completed({ outcome: 'refused', reason: 'not_exit', unknownCode: null }));
  o.controls.set('peer', { volume: 45, muted: false }, true);
  await vi.advanceTimersByTimeAsync(100);
  expect(o.failures).toEqual(['not_exit']);
  expect(o.targets.at(-1)).toBeUndefined();
});

it('owns remote completion errors so a retried stale revision never reaches the global toast', async () => {
  const { createAgentStore } = await import('../api/store');
  const store = createAgentStore();
  let facts = { token: 'v1', volume: 20, muted: false, exit: false };
  const sent: string[] = [];
  const errors: (string | null)[] = [];
  const controls = new SpeakerControls({
    read: () => facts,
    send: async id => { sent.push(id); return { type: 'device_control_accepted', data: { requestId: id } }; },
    onTargets: () => {},
    onFailure: reason => store.fail(reason),
    newId: () => `remote-${sent.length}`,
  });
  const off = store.onEvent(event => {
    if (event.type === 'device_control_completed') return controls.complete(event.data.requestId, event.data.result);
  });
  const unsubscribe = store.subscribe(() => errors.push(store.get().error));
  controls.set('peer', { volume: 60, muted: false }, true);
  await vi.advanceTimersByTimeAsync(0);
  store.receive({ type: 'device_control_completed', data: { requestId: sent[0], result: { outcome: 'refused', reason: 'stale_revision', unknownCode: null } } });
  expect(store.get().error).toBeNull();
  facts = { ...facts, token: 'v2', volume: 30 };
  controls.refresh();
  await vi.advanceTimersByTimeAsync(40);
  expect(sent).toHaveLength(2);
  store.receive({ type: 'device_control_completed', data: { requestId: sent[1], result: { outcome: 'failed', reason: 'apply_failed', unknownCode: null } } });
  expect(store.get().error).toBe('apply_failed');
  expect(errors).not.toContain('stale_revision');
  // An unrelated request still reports a stale refusal normally.
  store.receive({ type: 'device_control_completed', data: { requestId: 'other', result: { outcome: 'refused', reason: 'stale_revision', unknownCode: null } } });
  expect(store.get().error).toBe('stale_revision');
  off(); unsubscribe(); controls.dispose();
});

it('opts in only for a drag and flushes the release even after cards already show its value', async () => {
  let facts = { token: 'v1', volume: 20, muted: false, exit: false };
  const sent: { volume: number; deferred: boolean }[] = [];
  let revision = 1;
  const controls = new SpeakerControls({
    read: () => facts,
    send: async (_id, _facts, target, deferred) => {
      sent.push({ volume: target.volume, deferred });
      const changed = target.volume !== facts.volume;
      if (changed) queueMicrotask(() => {
        facts = { ...facts, ...target, token: `v${++revision}` };
        controls.refresh();
      });
      return completed({ outcome: changed ? 'applied' : 'no_change' });
    },
    onTargets: () => {}, onFailure: () => { throw Error('unexpected failure'); },
  });
  controls.set('peer', { volume: 60, muted: false }, false, true);
  await vi.advanceTimersByTimeAsync(100);
  expect(facts.volume).toBe(60);
  expect(sent).toEqual([{ volume: 60, deferred: true }]);
  controls.set('peer', { volume: 60, muted: false }, true);
  await vi.advanceTimersByTimeAsync(0);
  expect(sent).toEqual([{ volume: 60, deferred: true }, { volume: 60, deferred: false }]);
  controls.set('peer', { volume: 61, muted: false }); // keyboard/wheel default
  await vi.advanceTimersByTimeAsync(100);
  expect(sent.at(-1)).toEqual({ volume: 61, deferred: false });
  controls.dispose();
});
