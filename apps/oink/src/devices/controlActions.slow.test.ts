import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import type { DeviceControlReply } from '../api/cards';
import { ControlActions, SEND_INTERVAL_MS, type ControlAction } from './controlActions';

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

const volume = (value: number, extra: Partial<Extract<ControlAction, { kind: 'speakerVolume' }>> = {}): ControlAction =>
  ({ kind: 'speakerVolume', output: false, volume: value, muted: false, ...extra });

function owner() {
  const facts = { token: 'v1', action: volume(40) };
  const writes: { id: string; action: ControlAction; token: string }[] = [];
  let revision = 1;
  let reply: (id: string, action: ControlAction) => Promise<DeviceControlReply> = async requestId =>
    ({ type: 'device_control_accepted', data: { requestId } });
  const controller = new ControlActions({
    read: (_device, action) => ({ token: facts.token, matches: JSON.stringify(facts.action) === JSON.stringify(action) }),
    send: (id, _device, action) => { writes.push({ id, action, token: facts.token }); return reply(id, action); },
    refresh: async () => {}, show: () => {}, fail: () => {},
  });
  const report = (action: ControlAction, token: string) => { facts.action = action; facts.token = token; controller.snapshot(); };
  return {
    controller, writes,
    setReply(next: typeof reply) { reply = next; },
    report,
    nextToken() { return `v${++revision}`; },
  };
}

it('bounds a fast volume drag and sends the durable release immediately', async () => {
  const o = owner();
  o.setReply(async (id, action) => {
    queueMicrotask(() => o.report(action, o.nextToken()));
    return { type: 'device_control_completed', data: { requestId: id, result: { outcome: 'applied' } } };
  });
  for (let i = 0; i < 100; i++) {
    o.controller.set('a', volume(51 + i % 50, { deferPersistence: true }));
    await vi.advanceTimersByTimeAsync(8);
  }
  o.controller.set('a', volume(137, { final: true }));
  await vi.advanceTimersByTimeAsync(0);
  expect(o.writes.at(-1)!.action).toEqual(volume(137, { final: true }));
  expect(o.writes.length).toBeLessThanOrEqual(Math.ceil(800 / SEND_INTERVAL_MS) + 2);
  expect(o.writes.length).toBeGreaterThan(5);
  expect(new Set(o.writes.map(w => w.token)).size).toBe(o.writes.length);
  expect(o.controller.targets().size).toBe(0);
});
