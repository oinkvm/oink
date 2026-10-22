import type { DeviceControlReply, DeviceControlResult } from '../api/cards';

/** A volume/mute position for one device's forwarding or exit controls. */
export interface SpeakerTarget { volume: number; muted: boolean }
/** What the current device cards allow for one device: the version fence and its actual position. */
export interface SpeakerFacts extends SpeakerTarget { token: string; exit: boolean }

export interface SpeakerControlsDeps {
  /** Current card facts, read at send time; null when the device can no longer be set. */
  read: (device: string) => SpeakerFacts | null;
  send: (requestId: string, facts: SpeakerFacts, target: SpeakerTarget, deferPersistence: boolean) => Promise<DeviceControlReply>;
  /** Positions the UI shows while a device's controls are busy (latest request, not the round trip). */
  onTargets: (targets: ReadonlyMap<string, SpeakerTarget>) => void;
  onFailure: (reason: string) => void;
  newId?: () => string;
}

/**
 * Minimum spacing between two sends for one device. A local owner answers within
 * a millisecond, so without it every pointer move would reach the backend.
 */
export const SEND_INTERVAL_MS = 40;

interface State {
  shown: SpeakerTarget | null; // latest requested position, shown until the cards catch up
  desired: SpeakerTarget | null; // not yet sent
  inflight: string | null; // at most one request per device
  sent: SpeakerTarget | null;
  sentToken: string | null;
  waitToken: boolean; // the fence used last is spent; the next send needs the refreshed card token
  flush: boolean;
  deferPersistence: boolean;
  sentDeferred: boolean;
  needsDurable: boolean;
  timer: ReturnType<typeof setTimeout> | null;
}

/**
 * Live volume and mute for the sidebar sliders. Every device has at most one request in
 * flight; later positions replace each other while it runs, so only the newest is sent
 * next and an older value can never land after a newer one. The owner fences each
 * write with a revision (stale_revision otherwise), so after an applied write the next
 * one waits for the card snapshot that carries the new version token.
 */
export class SpeakerControls {
  private states = new Map<string, State>();
  // Kept across idle periods so a new drag right after a send still respects the spacing.
  private lastSentAt = new Map<string, number>();
  constructor(private deps: SpeakerControlsDeps) {}

  /** Request a position. `flush` skips the send spacing (pointer release, mute). */
  set(device: string, target: SpeakerTarget, flush = false, deferPersistence = false) {
    const s = this.state(device);
    s.shown = target;
    s.desired = target;
    s.deferPersistence = deferPersistence;
    if (flush) {
      s.flush = true;
      if (s.timer) { clearTimeout(s.timer); s.timer = null; }
    }
    this.publish();
    this.pump(device);
  }
  /** The latest position for a device: the pending request, else the card. */
  current(device: string): SpeakerTarget | null {
    const s = this.states.get(device);
    if (s?.shown) return s.shown;
    const facts = this.deps.read(device);
    return facts && { volume: facts.volume, muted: facts.muted };
  }
  /** Device cards changed: a waiting device may have its new version token now. */
  refresh() { for (const device of [...this.states.keys()]) this.pump(device); }
  /** A completion delivered as an event (the send replied `device_control_accepted`). */
  complete(requestId: string, result: DeviceControlResult) {
    for (const [device, s] of this.states) if (s.inflight === requestId) {
      this.done(device, requestId, result);
      return true; // This controller owns retry and final failure presentation.
    }
    return false;
  }
  dispose() { for (const s of this.states.values()) if (s.timer) clearTimeout(s.timer); this.states.clear(); this.lastSentAt.clear(); }

  private state(device: string) {
    let s = this.states.get(device);
    if (!s) this.states.set(device, s = { shown: null, desired: null, inflight: null, sent: null, sentToken: null, waitToken: false, flush: false, deferPersistence: false, sentDeferred: false, needsDurable: false, timer: null });
    return s;
  }
  private publish() {
    this.deps.onTargets(new Map([...this.states].flatMap(([device, s]) => s.shown ? [[device, s.shown] as const] : [])));
  }
  private idle(device: string) {
    const s = this.states.get(device);
    if (s?.timer) clearTimeout(s.timer);
    this.states.delete(device);
    this.publish();
  }
  private pump(device: string) {
    const s = this.states.get(device);
    if (!s || s.inflight || s.timer) return;
    const facts = this.deps.read(device);
    if (!facts) return this.idle(device);
    if (s.waitToken && facts.token === s.sentToken) return;
    s.waitToken = false;
    const target = s.desired;
    if (!target || (target.volume === facts.volume && target.muted === facts.muted && (!s.needsDurable || s.deferPersistence))) {
      if (s.needsDurable) {
        // Retain the pending durability obligation even after cards catch up.
        // Pointer release must send a synchronous no-change flush if necessary.
        s.shown = null;
        s.desired = null;
        this.publish();
        return;
      }
      return this.idle(device);
    }
    const wait = (this.lastSentAt.get(device) ?? -Infinity) + SEND_INTERVAL_MS - Date.now();
    if (wait > 0 && !s.flush) {
      s.timer = setTimeout(() => { s.timer = null; this.pump(device); }, wait);
      return;
    }
    const id = this.deps.newId?.() ?? crypto.randomUUID();
    const deferPersistence = !facts.exit && s.deferPersistence;
    Object.assign(s, { inflight: id, sent: target, sentToken: facts.token, desired: null, flush: false, sentDeferred: deferPersistence });
    if (deferPersistence) s.needsDurable = true;
    this.lastSentAt.set(device, Date.now());
    this.deps.send(id, facts, target, deferPersistence).then(
      reply => { if (reply.type === 'device_control_completed') this.done(device, id, reply.data.result); },
      () => this.done(device, id, { outcome: 'failed', reason: 'outcome_unknown', unknownCode: null }),
    );
  }
  private done(device: string, id: string, result: DeviceControlResult) {
    const s = this.states.get(device);
    if (!s || s.inflight !== id) return;
    s.inflight = null;
    if ((result.outcome === 'applied' || result.outcome === 'no_change') && !s.sentDeferred) s.needsDurable = false;
    if (result.outcome === 'applied') s.waitToken = true;
    else if (result.outcome === 'refused' || result.outcome === 'failed') {
      if (result.reason === 'stale_revision') {
        // Someone else (a volume key, another device) moved it first: retry the newest
        // position once the card carries the owner's new version.
        s.waitToken = true;
        s.desired ??= s.sent;
      } else {
        this.deps.onFailure(result.reason);
        if (!s.desired) return this.idle(device);
      }
    }
    this.pump(device);
  }
}
