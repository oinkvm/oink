import type { DeviceKey, Layout, Point, Rect, ScreenKey } from './types';
import type { Transport } from './transport';
import type { ProductEvent } from './store';
import {
  createLayoutProfilesApi, LayoutProfileFailure,
  type Adjustment, type ArrangementSnapshot, type CommitRequest, type DeviceArrangementState,
  type LayoutOperation, type LayoutProfilesState, type NativeState, type PreviewReply,
  effectiveSystemSync, type PreviewRequest, type ProfileError, type ProfileName, type ReplayRequest,
} from './layoutProfiles';

const copy = <T>(value: T): T => structuredClone(value);
const key = (screen: ScreenKey) => JSON.stringify([screen.device, screen.display]);
const compare = (a: string, b: string) => a < b ? -1 : a > b ? 1 : 0;
const EPS = 1e-10;
const failure = (code: ProfileError['code'], message: string, device?: string): never => {
  throw new LayoutProfileFailure({ code, message, ...(device ? { device } : {}) });
};
const nativeGeometry = (native: NativeState) => ({
  device: native.device, primary: native.primary,
  screens: [...native.screens].sort((a, b) => compare(key(a.screen), key(b.screen))),
});
/** Fixture equality follows the contract, ignoring tokens, selection and live availability. */
export function sameProfileContent(a: ArrangementSnapshot, b: ArrangementSnapshot): boolean {
  const canonical = (s: ArrangementSnapshot) => ({
    placements: [...s.placements].sort((x, y) => compare(x.device, y.device)),
    disabled: [...s.disabled].sort((x, y) => compare(key(x.screen), key(y.screen))),
    native: [...s.native].sort((x, y) => compare(x.device, y.device)).map(nativeGeometry),
    screen_shared: [...s.placements].sort((x,y) => compare(x.device,y.device)).map(p => ({device:p.device,shared:s.screen_shared?.find(d=>d.device===p.device)?.shared ?? true})),
    topology: s.topology,
  });
  return equal(canonical(a), canonical(b));
}
function equal(a: unknown, b: unknown): boolean {
  if (typeof a === 'number' && typeof b === 'number') return Number.isFinite(a) && Number.isFinite(b)
    && Math.abs(a - b) <= EPS * Math.max(1, Math.abs(a), Math.abs(b));
  if (a === b) return true;
  if (!a || !b || typeof a !== 'object' || typeof b !== 'object') return false;
  if (Array.isArray(a) || Array.isArray(b)) return Array.isArray(a) && Array.isArray(b)
    && a.length === b.length && a.every((v, i) => equal(v, b[i]));
  const x = a as Record<string, unknown>, y = b as Record<string, unknown>;
  const keys = Object.keys(x).sort();
  return equal(keys, Object.keys(y).sort()) && keys.every(k => equal(x[k], y[k]));
}
const overlap = (a: Rect, b: Rect) => Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x) > EPS
  && Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y) > EPS;
const contact = (a: Rect, b: Rect) =>
  ((a.x + a.w === b.x || b.x + b.w === a.x) && Math.min(a.y + a.h, b.y + b.h) > Math.max(a.y, b.y))
  || ((a.y + a.h === b.y || b.y + b.h === a.y) && Math.min(a.x + a.w, b.x + b.w) > Math.max(a.x, b.x));
const rects = (n: NativeState) => n.screens.map(s => ({ x: s.origin.x, y: s.origin.y, ...s.size }));
function connected(rs: Rect[]): boolean {
  if (!rs.length) return false;
  if (rs.some(r => ![r.x, r.y, r.w, r.h].every(Number.isSafeInteger) || r.w <= 0 || r.h <= 0)) return false;
  if (rs.some((a, i) => rs.slice(i + 1).some(b => overlap(a, b)))) return false;
  const seen = new Set([0]);
  for (let previous = -1; previous !== seen.size;) {
    previous = seen.size;
    rs.forEach((a, i) => { if ([...seen].some(j => contact(a, rs[j]))) seen.add(i); });
  }
  return seen.size === rs.length;
}
/** A fake native planner: no OS calls. Backend remains the production constraint owner. */
function snap(native: NativeState, screen: ScreenKey, desired: Point): NativeState {
  const out = copy(native), index = out.screens.findIndex(s => key(s.screen) === key(screen));
  if (index < 0) return failure('invalid_geometry', 'Unknown native screen', native.device);
  const moving = out.screens[index], stationary = out.screens.filter((_, i) => i !== index);
  const round = (x: number) => Math.sign(x) * Math.round(Math.abs(x));
  desired = { x: round(desired.x), y: round(desired.y) };
  if (![desired.x, desired.y].every(Number.isSafeInteger)) return failure('invalid_geometry', 'Invalid origin');
  const xs = new Set([desired.x]), ys = new Set([desired.y]);
  for (const s of stationary) {
    xs.add(s.origin.x - moving.size.w); xs.add(s.origin.x + s.size.w);
    xs.add(s.origin.x + 1 - moving.size.w); xs.add(s.origin.x + s.size.w - 1);
    ys.add(s.origin.y - moving.size.h); ys.add(s.origin.y + s.size.h);
    ys.add(s.origin.y + 1 - moving.size.h); ys.add(s.origin.y + s.size.h - 1);
  }
  const choices = [...xs].flatMap(x => [...ys].map(y => ({ x, y })))
    .sort((a, b) => (a.x - desired.x) ** 2 + (a.y - desired.y) ** 2
      - ((b.x - desired.x) ** 2 + (b.y - desired.y) ** 2) || a.x - b.x || a.y - b.y);
  for (const origin of choices) {
    moving.origin = origin;
    if (connected(rects(out))) return out;
  }
  return failure('disconnected', 'No edge-connected native position', native.device);
}
function visibleRects(snapshot: ArrangementSnapshot, device: DeviceKey): Rect[] {
  const p = snapshot.placements.find(p => p.device === device);
  if (!p || snapshot.screen_shared?.some(d => d.device === device && !d.shared)) return [];
  return snapshot.native.find(n => n.device === device)?.screens
    .filter(s => !snapshot.disabled.some(d => key(d.screen) === key(s.screen) && d.disabled))
    .map(s => ({ x: p.origin.x + s.origin.x * p.scale, y: p.origin.y + s.origin.y * p.scale,
      w: s.size.w * p.scale, h: s.size.h * p.scale })) ?? [];
}
function repairCollisions(snapshot: ArrangementSnapshot, order: string[], adjustments: Adjustment[]) {
  const placed: Rect[] = [];
  for (const device of order) {
    let moving = visibleRects(snapshot, device);
    if (moving.some(a => placed.some(b => overlap(a, b)))) {
      const xs = new Set([0]), ys = new Set([0]);
      for (const a of moving) for (const b of placed) {
        xs.add(b.x + b.w - a.x); xs.add(b.x - a.x - a.w);
        ys.add(b.y + b.h - a.y); ys.add(b.y - a.y - a.h);
      }
      const choices = [...xs].flatMap(x => [...ys].map(y => ({ x, y })))
        .sort((a, b) => a.x ** 2 + a.y ** 2 - b.x ** 2 - b.y ** 2 || a.x - b.x || a.y - b.y);
      const shift = choices.find(p => moving.every(a => placed.every(b => !overlap({ ...a, x: a.x + p.x, y: a.y + p.y }, b))));
      if (!shift) return failure('no_valid_placement', 'No collision-free placement', device);
      const p = snapshot.placements.find(p => p.device === device)!;
      p.origin.x += shift.x; p.origin.y += shift.y;
      adjustments.push({ code: 'overlap_repaired', device });
      moving = visibleRects(snapshot, device);
    }
    placed.push(...moving);
  }
}

export function layoutProfilesFixtureSnapshot(): ArrangementSnapshot {
  const native = (device: string, side: boolean): NativeState => ({
    device, token: `native:${device}:1`, primary: { device, display: 'main' },
    screens: [
      { screen: { device, display: 'main' }, origin: { x: 0, y: 0 }, size: { w: 1200, h: 800 } },
      ...(side ? [{ screen: { device, display: 'side' }, origin: { x: 1200, y: 0 }, size: { w: 800, h: 600 } }] : []),
    ],
  });
  const devices = ['a'.repeat(64), 'b'.repeat(64), 'c'.repeat(64)];
  const natives = devices.map((d, i) => native(d, i < 2));
  return {
    active_profile_id: '00000000000000000000000000000001',
    placements: devices.map((device, i) => ({ device, origin: { x: i * 2.5, y: 0 }, scale: .001 })),
    disabled: natives.flatMap(n => n.screens.map(s => ({ screen: s.screen, disabled: false }))),
    native: natives,
    topology: { gap_travel: true, walls: true, overlap_tolerance: 0, minimum_portal: .01, portal_snap: 0, endpoint_reach: 0 },
  };
}

interface Profile { id: string; name: ProfileName; content_revision: string; last_used: bigint; content: ArrangementSnapshot; match: string }
/** 0.2.0 catalog bounds (docs/design/layout-profiles.md). */
export const PROFILE_LIMIT = 16;
export const NAME_LIMIT = 32;
export interface LayoutProfilesFixtureOptions {
  snapshot?: ArrangementSnapshot;
  devices?: DeviceArrangementState[];
  now?: () => number;
  fallback?: Transport;
}

/** In-memory contract transport for UI development. It never imports a native API. */
export function createLayoutProfilesFixture(options: LayoutProfilesFixtureOptions = {}) {
  let snapshot = copy(options.snapshot ?? layoutProfilesFixtureSnapshot());
  let devices: DeviceArrangementState[] = copy(options.devices ?? snapshot.native.map(n => ({
    device: n.device, sync_system_arrangement: false, preference_token: `preference:${n.device}:1`,
    native_token: n.token, availability: 'ready', can_edit_screens: false,
  })));
  const initialScales = new Map(snapshot.placements.map(p => [p.device, p.scale]));
  let sequence = 1n, catalog = 1n, clock = 0n, nextId = 2n, revision = 1;
  let operation: LayoutOperation | null = null;
  const listeners = new Set<(event: ProductEvent) => void>();
  const results = new Map<string, { signature: string; result: unknown }>();
  const operations = new Map<string, LayoutOperation>();
  const plans = new Map<string, { state: string; preview: PreviewReply }>();
  const failures = new Map<string, ProfileError>();
  const work = new Set<Promise<void>>();
  const now = () => { clock = [clock + 1n, BigInt(Math.floor((options.now ?? Date.now)()))].reduce((a, b) => a > b ? a : b); return clock; };
  const match = () => JSON.stringify(devices.map(d => ({ device: d.device,
    screens: snapshot.native.find(n => n.device === d.device)?.screens.map(s => s.screen.display).sort() ?? null,
  })).sort((a, b) => compare(a.device, b.device)));
  let profiles: Profile[] = [{ id: snapshot.active_profile_id, name: { kind: 'default' },
    content_revision: '1', last_used: now(), content: copy(snapshot), match: match() }];
  const token = () => `fixture:${sequence}`;
  const active = () => profiles.find(p => p.id === snapshot.active_profile_id)!;
  const busy = () => operation?.status === 'running' || operation?.status === 'outcome_unknown';
  /** Pure most-recent-use order, descending, then ascending ID; matching never reorders. */
  const mru = () => [...profiles].sort((a, b) => a.last_used > b.last_used ? -1 : a.last_used < b.last_used ? 1 : compare(a.id, b.id));
  function state(): LayoutProfilesState {
    const currentMatch = match();
    return copy({
      epoch: 'layout-profiles-fixture', sequence: String(sequence), token: token(), catalog_token: String(catalog), readiness: 'ready',
      profiles: mru().map(p => ({ id: p.id, name: p.name, content_revision: p.content_revision, last_used_ms: String(p.last_used),
        exact_match: p.match === currentMatch, active: p.id === snapshot.active_profile_id })),
      active_profile_id: snapshot.active_profile_id,
      create_enabled: !busy() && profiles.length < PROFILE_LIMIT, snapshot, devices,
      pending: operation?.owners.filter(o => o.phase === 'applying' || o.phase === 'unknown') ?? [], operation,
    });
  }
  const emit = (event: ProductEvent) => listeners.forEach(fn => fn(copy(event)));
  function changed(catalogChanged = false) {
    ++sequence; if (catalogChanged) ++catalog;
    emit({ type: 'layout_profiles', data: state() });
  }
  function operationEvent() {
    emit({ type: 'layout_profile_operation', data: { epoch: 'layout-profiles-fixture', sequence: String(sequence), operation: copy(operation!), state: state() } });
  }
  function check(params: Record<string, unknown>, onlyCatalog = false) {
    if (busy()) return failure('busy', 'An arrangement operation is still pending');
    if (params[onlyCatalog ? 'expected_catalog_token' : 'expected_token'] !== (onlyCatalog ? String(catalog) : token())) {
      return failure('stale_state', 'Refresh the layout profile state');
    }
  }
  /** Trimmed, nonempty, at most 32 characters, no controls, unique case-insensitively;
   * the default's logical name is `Default layout`. */
  function named(value: unknown, except?: string): ProfileName {
    if (typeof value !== 'string') return failure('invalid_name', 'A profile name is required');
    const name = value.trim();
    if (!name || [...name].length > NAME_LIMIT || /[\u0000-\u001f\u007f-\u009f]/u.test(name)) return failure('invalid_name', 'Invalid profile name');
    const logical = (n: ProfileName) => (n.kind === 'default' ? 'Default layout' : n.value).toLocaleLowerCase();
    if (profiles.some(p => p.id !== except && logical(p.name) === name.toLocaleLowerCase())) return failure('duplicate_name', 'A profile with this name exists');
    return { kind: 'custom', value: name };
  }
  /** A manual change saves only its own delta into the current profile, keeping saved
   * fields that differ from live only through automatic adjustments. */
  function autosave(before: ArrangementSnapshot, after: ArrangementSnapshot, nativeDevices: string[]) {
    const target = active(); if (!target) return;
    const content = target.content;
    for (const p of after.placements) {
      const old = before.placements.find(v => v.device === p.device);
      if (old && equal(old, p)) continue;
      content.placements = [...content.placements.filter(v => v.device !== p.device), copy(p)];
    }
    for (const bit of after.disabled) {
      const old = before.disabled.find(b => key(b.screen) === key(bit.screen));
      if (old?.disabled === bit.disabled) continue;
      content.disabled = [...content.disabled.filter(b => key(b.screen) !== key(bit.screen)), copy(bit)];
    }
    for (const sharing of after.screen_shared ?? []) {
      const old = before.screen_shared?.find(d => d.device === sharing.device)?.shared ?? true;
      if (old === sharing.shared) continue;
      content.screen_shared = [...(content.screen_shared ?? []).filter(d => d.device !== sharing.device), copy(sharing)];
    }
    for (const device of nativeDevices) {
      const n = after.native.find(v => v.device === device);
      if (n) content.native = [...content.native.filter(v => v.device !== device), copy(n)];
    }
    target.content_revision = String(++revision); target.last_used = now(); target.match = match();
  }
  const profile = (id: unknown) => profiles.find(p => p.id === id) ?? failure('profile_not_found', 'The profile no longer exists');
  function restore(saved: ArrangementSnapshot): { next: ArrangementSnapshot; adjustments: Adjustment[]; nativeDevices: string[] } {
    const next = copy(snapshot), adjustments: Adjustment[] = [], nativeDevices: string[] = [], changedDevices: string[] = [];
    next.topology = copy(saved.topology);
    for (const p of saved.placements) if (!devices.some(d => d.device === p.device)) adjustments.push({ code: 'device_removed', device: p.device });
    for (const device of devices) {
      const current = next.native.find(n => n.device === device.device)!;
      const stored = saved.native.find(n => n.device === device.device);
      const placement = next.placements.find(p => p.device === device.device)!;
      const savedPlacement = saved.placements.find(p => p.device === device.device);
      if (!stored || !savedPlacement) { adjustments.push({ code: 'device_added', device: device.device }); continue; }
      Object.assign(placement, copy(savedPlacement));
      for (const s of stored.screens) if (!current.screens.some(c => key(c.screen) === key(s.screen))) adjustments.push({ code: 'screen_removed', device: device.device, screen: s.screen });
      for (const s of current.screens) if (!stored.screens.some(c => key(c.screen) === key(s.screen))) adjustments.push({ code: 'screen_added', device: device.device, screen: s.screen });
      if (device.availability === 'offline') {
        next.native[next.native.indexOf(current)] = copy(stored);
        for (const s of current.screens) adjustments.push({ code: 'owner_offline', device: device.device, screen: s.screen });
        continue;
      }
      const main = current.screens.find(s => key(s.screen) === key(current.primary))!;
      const savedMain = stored.screens.find(s => key(s.screen) === key(current.primary))
        ?? stored.screens.find(s => key(s.screen) === key(stored.primary))!;
      if (key(current.primary) !== key(stored.primary)) adjustments.push({ code: 'anchor_changed', device: device.device });
      placement.origin = { x: savedPlacement.origin.x + (savedMain.origin.x - main.origin.x) * placement.scale,
        y: savedPlacement.origin.y + (savedMain.origin.y - main.origin.y) * placement.scale };
      if (!equal(nativeGeometry(current), nativeGeometry(stored))) {
        changedDevices.push(device.device); adjustments.push({ code: 'native_changed', device: device.device });
      }
      if (effectiveSystemSync(device.sync_system_arrangement, true) && device.availability === 'ready') {
        let assembled: NativeState = { ...copy(current), screens: [{ ...copy(main), origin: { x: 0, y: 0 } }] };
        for (const s of current.screens.filter(s => key(s.screen) !== key(main.screen)).sort((a, b) => compare(key(a.screen), key(b.screen)))) {
          const old = stored.screens.find(o => key(o.screen) === key(s.screen));
          const desired = old ? { x: old.origin.x - savedMain.origin.x, y: old.origin.y - savedMain.origin.y } : s.origin;
          assembled.screens.push(copy(s)); assembled = snap(assembled, s.screen, desired);
        }
        if (!equal(nativeGeometry(assembled), nativeGeometry(current))) nativeDevices.push(device.device);
        next.native[next.native.indexOf(current)] = assembled;
      }
      next.screen_shared = [...(next.screen_shared ?? []).filter(d => d.device !== device.device), { device: device.device, shared: saved.screen_shared?.find(d => d.device === device.device)?.shared ?? true }];
      for (const bit of next.disabled.filter(b => b.screen.device === device.device)) {
        const storedBit = saved.disabled.find(b => key(b.screen) === key(bit.screen));
        if (storedBit) bit.disabled = storedBit.disabled;
      }
    }
    const rank = (d: string) => changedDevices.includes(d) ? 0 : saved.placements.some(p => p.device === d) ? 1 : 2;
    repairCollisions(next, devices.map(d => d.device).sort((a, b) => rank(a) - rank(b) || compare(a, b)), adjustments);
    return { next, adjustments, nativeDevices };
  }
  function start(id: string, kind: LayoutOperation['kind'], next: ArrangementSnapshot, adjustments: Adjustment[], nativeDevices: string[], before = copy(snapshot), manual = kind === 'edit' || kind === 'reset') {
    operation = { id, kind, status: 'running', before, after: null, native_restore_devices: [], adjustments,
      owners: (kind === 'reset' ? [] : devices).map(d => ({ device: d.device, phase: d.availability === 'offline' ? 'skipped_offline' : 'applying', error: null })), history_required: false, error: null };
    operations.set(id, operation); changed(); operationEvent();
    const accepted = { operation_id: id, state: state() };
    const pending = new Promise<void>(resolve => setTimeout(() => {
      const op = operations.get(id)!;
      for (const owner of op.owners) {
        const injected = failures.get(owner.device);
        failures.delete(owner.device);
        if (injected) {
          owner.phase = injected.code === 'outcome_unknown' ? 'unknown' : 'failed'; owner.error = injected;
          const old = before.native.find(n => n.device === owner.device);
          const index = next.native.findIndex(n => n.device === owner.device);
          if (old && index >= 0) next.native[index] = copy(old);
          next.screen_shared = [...(next.screen_shared ?? []).filter(d => d.device !== owner.device), { device: owner.device, shared: before.screen_shared?.find(d => d.device === owner.device)?.shared ?? true }];
          for (const bit of next.disabled.filter(b => b.screen.device === owner.device)) {
            bit.disabled = before.disabled.find(b => key(b.screen) === key(bit.screen))?.disabled ?? bit.disabled;
          }
        } else if (owner.phase !== 'skipped_offline') owner.phase = 'applied';
      }
      snapshot = copy(next);
      for (const d of devices) {
        const n = snapshot.native.find(n => n.device === d.device);
        if (n) { n.token = `native:${d.device}:${sequence + 1n}`; d.native_token = n.token; }
      }
      op.after = copy(snapshot);
      op.history_required = !sameProfileContent(before, snapshot) || before.active_profile_id !== snapshot.active_profile_id;
      op.native_restore_devices = nativeDevices.filter(d => op.owners.some(o => o.device === d && o.phase === 'applied'));
      op.error = op.owners.find(o => o.error)?.error ?? null;
      op.status = op.owners.some(o => o.phase === 'unknown') ? 'outcome_unknown'
        : op.error ? op.history_required ? 'partial' : 'failed' : 'applied';
      // Auto-save is part of a manual operation's terminal success; a switch never saves.
      if (manual && op.status !== 'failed' && op.status !== 'outcome_unknown') autosave(before, snapshot, op.native_restore_devices);
      changed(manual); emit({ type: 'layout', data: layout() }); operationEvent(); resolve();
    }, 0));
    work.add(pending); void pending.then(() => work.delete(pending));
    return accepted;
  }
  function preview(request: PreviewRequest): PreviewReply {
    if (request.expected_token !== token()) return failure('stale_state', 'Refresh before preview');
    const device = devices.find(d => d.device === request.device) ?? failure('not_member', 'Unknown device');
    if (device.availability !== 'ready') return failure(device.availability === 'offline' ? 'owner_offline' : 'unsupported', 'Owner cannot arrange', device.device);
    if (!device.sync_system_arrangement) return failure('setting_disabled', 'System arrangement is disabled', device.device);
    const source = snapshot.native.find(n => n.device === request.device)!;
    const placements = copy(snapshot.placements), placement = placements.find(p => p.device === request.device)!;
    let native = snap(source, request.moved_screen, { x: (request.desired_canvas_origin.x - placement.origin.x) / placement.scale,
      y: (request.desired_canvas_origin.y - placement.origin.y) / placement.scale });
    const main = native.screens.find(s => key(s.screen) === key(native.primary))!;
    const offset = copy(main.origin);
    native = { ...native, screens: native.screens.map(s => ({ ...s, origin: { x: s.origin.x - offset.x, y: s.origin.y - offset.y } })) };
    placement.origin.x += offset.x * placement.scale; placement.origin.y += offset.y * placement.scale;
    const result: PreviewReply = { sequence: request.sequence, native, placements,
      plan_token: `plan:${sequence}:${plans.size}`, adjustments: [{ code: 'native_snapped', device: request.device, screen: request.moved_screen }] };
    plans.set(result.plan_token, { state: token(), preview: copy(result) });
    return result;
  }
  function layout(): Layout {
    const screens = snapshot.native.flatMap(n => {
      const p = snapshot.placements.find(p => p.device === n.device)!;
      return n.screens.map(s => ({ key: s.screen, device_name: `Fixture ${n.device.slice(0, 1)}`, os: 'macos' as const,
        display: { key: s.screen.display, name: s.screen.display, bounds: { ...s.origin, ...s.size },
          pixel_width: s.size.w, pixel_height: s.size.h, reference_extent: null, rotation_deg: 0,
          is_primary: key(s.screen) === key(n.primary), is_builtin: false, is_virtual: false },
        origin: { x: p.origin.x + s.origin.x * p.scale, y: p.origin.y + s.origin.y * p.scale },
        size: { w: s.size.w * p.scale, h: s.size.h * p.scale }, size_source: 'manual' as const,
        gain: 1, hidden: snapshot.disabled.find(b => key(b.screen) === key(s.screen))?.disabled ?? false,
        device_disabled: !(snapshot.screen_shared?.find(d => d.device === n.device)?.shared ?? true), online: devices.find(d => d.device === n.device)?.availability !== 'offline',
      }));
    });
    const xs = screens.map(s => s.origin.x), ys = screens.map(s => s.origin.y);
    const x = Math.min(0, ...xs), y = Math.min(0, ...ys);
    return { revision: Number(sequence), source: 'manual', geometry: { screens,
      bounds: { x, y, w: Math.max(0, ...screens.map(s => s.origin.x + s.size.w)) - x,
        h: Math.max(0, ...screens.map(s => s.origin.y + s.size.h)) - y } } };
  }
  function execute(method: string, p: Record<string, unknown>): unknown {
    if (method === 'layout_profiles') return state();
    if (method === 'layout') return layout();
    if (method === 'layout_profile_operation') {
      const op = operations.get(String(p.request_id)) ?? failure('stale_state', 'Unknown request ID');
      return { operation: copy(op), state: state() };
    }
    if (method === 'preview_system_arrangement') return preview(p as unknown as PreviewRequest);
    if (method === 'set_sync_system_arrangement') {
      if (busy()) return failure('busy', 'An arrangement operation is pending');
      const d = devices.find(d => d.device === p.device) ?? failure('not_member', 'Unknown device');
      if (d.availability !== 'ready') return failure(d.availability === 'offline' ? 'owner_offline' : 'unsupported', 'Owner unavailable');
      if (d.preference_token !== p.expected_preference_token) return failure('stale_state', 'Refresh the owner preference');
      if (typeof p.enabled !== 'boolean') return failure('invalid_geometry', 'Expected a boolean setting');
      d.sync_system_arrangement = p.enabled; d.can_edit_screens = effectiveSystemSync(p.enabled, true);
      d.preference_token = `preference:${d.device}:${sequence + 1n}`; changed(); return state();
    }
    const catalogOnly = method === 'rename_layout_profile' || method === 'delete_layout_profile';
    check(p, catalogOnly);
    if (method === 'create_layout_profile') {
      if (profiles.length >= PROFILE_LIMIT) return failure('profile_limit', 'At most sixteen profiles');
      const name = named(p.name), id = (nextId++).toString(16).padStart(32, '0');
      snapshot.active_profile_id = id;
      profiles.push({ id, name, content_revision: String(++revision), last_used: now(), content: copy(snapshot), match: match() });
      changed(true); return state();
    }
    if (method === 'rename_layout_profile') { const target = profile(p.profile_id); target.name = named(p.name, target.id); changed(true); return state(); }
    if (method === 'delete_layout_profile') {
      profile(p.profile_id);
      if (profiles.length === 1) return failure('last_profile', 'The last profile cannot be deleted');
      profiles = profiles.filter(profile => profile.id !== p.profile_id);
      // Deleting the current profile selects the first remaining MRU entry without loading it.
      if (!profiles.some(profile => profile.id === snapshot.active_profile_id)) snapshot.active_profile_id = mru()[0].id;
      changed(true); return state();
    }
    if (method === 'load_layout_profile') {
      const stored = profile(p.profile_id), before = copy(snapshot);
      const { next, adjustments, nativeDevices } = restore(stored.content);
      snapshot.active_profile_id = stored.id; next.active_profile_id = stored.id;
      stored.last_used = now(); changed(true);
      return start(String(p.request_id), 'load', next, adjustments, nativeDevices, before);
    }
    if (method === 'reset_arrangement') {
      const next = copy(snapshot);
      // Model the backend's initial placement from current native rectangles.
      // There is no frontend compaction and no visibility/native restore.
      let right = 0, centerY = 0;
      for (const [index, p] of next.placements.entries()) {
        p.scale = initialScales.get(p.device) ?? p.scale;
        const n = next.native.find(n => n.device === p.device)!;
        const minX = Math.min(...n.screens.map(s => s.origin.x));
        const minY = Math.min(...n.screens.map(s => s.origin.y));
        const maxX = Math.max(...n.screens.map(s => s.origin.x + s.size.w));
        const maxY = Math.max(...n.screens.map(s => s.origin.y + s.size.h));
        const width = (maxX - minX) * p.scale, height = (maxY - minY) * p.scale;
        if (index === 0) centerY = height / 2;
        p.origin = { x: right - minX * p.scale, y: centerY - height / 2 - minY * p.scale };
        right += width;
      }
      return start(String(p.request_id), 'reset', next, [], []);
    }
    if (method === 'commit_arrangement') {
      const request = p as unknown as CommitRequest, next = copy(snapshot), nativeDevices: string[] = [];
      if (new Set(request.placements.map(v => v.device)).size !== request.placements.length) return failure('invalid_geometry', 'Duplicate placement');
      for (const value of request.placements) {
        if (![value.origin.x, value.origin.y, value.scale].every(Number.isFinite) || value.scale <= 0) return failure('invalid_geometry', 'Invalid placement');
        const current = next.placements.find(v => v.device === value.device) ?? failure('not_member', 'Unknown device');
        Object.assign(current, copy(value));
      }
      for (const planToken of request.native_plan_tokens) {
        const plan = plans.get(planToken);
        if (!plan || plan.state !== token()) return failure('stale_state', 'Native preview changed');
        next.native[next.native.findIndex(n => n.device === plan.preview.native.device)] = copy(plan.preview.native);
        nativeDevices.push(plan.preview.native.device);
      }
      for (const sharing of request.screen_shared ?? []) {
        const device = devices.find(d => d.device === sharing.device) ?? failure('not_member', 'Unknown device');
        if (device.availability === 'offline') return failure('owner_offline', 'Cannot edit offline visibility', device.device);
        next.screen_shared = [...(next.screen_shared ?? []).filter(d => d.device !== sharing.device), copy(sharing)];
      }
      for (const bit of request.disabled) {
        const device = devices.find(d => d.device === bit.screen.device);
        if (device?.availability === 'offline') return failure('owner_offline', 'Cannot edit offline visibility', device.device);
        const current = next.disabled.find(d => key(d.screen) === key(bit.screen)) ?? failure('invalid_geometry', 'Unknown screen');
        current.disabled = bit.disabled;
      }
      return start(String(p.request_id), 'edit', next, [], nativeDevices);
    }
    if (method === 'replay_arrangement') {
      const request = p as unknown as ReplayRequest;
      // Ignore native observations for a rigid step. Visibility/placement conflicts still fence replay.
      const source = copy(snapshot), expected = copy(request.from);
      source.native = source.native.filter(n => request.native_restore_devices.includes(n.device));
      expected.native = expected.native.filter(n => request.native_restore_devices.includes(n.device));
      if (!sameProfileContent(source, expected)) return failure('stale_state', 'History conflicts with current state');
      const next = copy(request.target);
      for (const n of next.native) {
        const current = snapshot.native.find(c => c.device === n.device)!;
        if (!request.native_restore_devices.includes(n.device)) { Object.assign(n, copy(current)); continue; }
        if (!devices.find(d => d.device === n.device)?.sync_system_arrangement) return failure('setting_disabled', 'System arrangement is disabled', n.device);
        if (!equal(n.screens.map(s => [key(s.screen), s.size]).sort(), current.screens.map(s => [key(s.screen), s.size]).sort())) return failure('native_changed', 'Native dimensions changed', n.device);
        if (!connected(rects(n))) return failure('disconnected', 'History native state is disconnected', n.device);
      }
      // A switch replays selection with live state (touching its recency, never its content);
      // an edit or reset replay keeps the current profile and auto-saves its delta there.
      const load = request.original_kind === 'load';
      if (!load || !profiles.some(profile => profile.id === next.active_profile_id)) next.active_profile_id = snapshot.active_profile_id;
      if (load) { snapshot.active_profile_id = next.active_profile_id; active().last_used = now(); }
      changed(true);
      return start(String(p.request_id), 'replay', next, [], request.native_restore_devices, copy(request.from), !load);
    }
    return failure('unsupported', `Fixture does not implement ${method}`);
  }
  const transport: Transport = {
    async call<R>({ method, params = {} }: { method: string; params?: Record<string, unknown> }): Promise<R> {
      if (!['layout_profiles', 'layout', 'layout_profile_operation', 'preview_system_arrangement', 'set_sync_system_arrangement',
        'create_layout_profile', 'rename_layout_profile', 'delete_layout_profile',
        'load_layout_profile', 'commit_arrangement', 'reset_arrangement', 'replay_arrangement'].includes(method) && options.fallback) return options.fallback.call({ method, params });
      try {
        const query = ['layout_profiles', 'layout', 'layout_profile_operation', 'preview_system_arrangement'].includes(method);
        const id = String(params.request_id ?? '');
        if (!query && !id) return failure('stale_state', 'A request ID is required');
        const signature = JSON.stringify({ method, params });
        const previous = !query && results.get(id);
        if (previous) {
          if (previous.signature !== signature) return failure('stale_state', 'Request ID reused with different payload');
          return copy(previous.result) as R;
        }
        const result = execute(method, params);
        if (!query) results.set(id, { signature, result: copy(result) });
        return copy(result) as R;
      } catch (error) {
        if (error instanceof LayoutProfileFailure) return { type: 'profile_error', error: error.problem } as R;
        throw error;
      }
    },
    shell: async <R>(command: string, args?: Record<string, unknown>): Promise<R> => options.fallback
      ? options.fallback.shell(command, args) : failure('unsupported', 'No native shell in a profile fixture'),
    async onEvent(handler) { listeners.add(handler); return () => { listeners.delete(handler); }; },
  };
  return {
    transport, api: createLayoutProfilesApi((method, params) => transport.call({ method, params })),
    state, layout,
    flush: async () => { await Promise.all([...work]); },
    setOnline(device: string, online: boolean) {
      const d = devices.find(d => d.device === device) ?? failure('not_member', 'Unknown device');
      d.availability = online ? 'ready' : 'offline'; d.can_edit_screens = effectiveSystemSync(d.sync_system_arrangement, online); changed();
    },
    /** Simulates native hotplug, remote layout/visibility edits, or group membership changes. */
    replaceCurrent(next: ArrangementSnapshot, nextDevices?: DeviceArrangementState[]) {
      if (busy()) return failure('busy', 'Wait for the operation before replacing fixture state');
      snapshot = copy(next);
      if (nextDevices) devices = copy(nextDevices);
      else devices = devices.filter(d => snapshot.placements.some(p => p.device === d.device));
      if (!profiles.some(p => p.id === snapshot.active_profile_id)) snapshot.active_profile_id = profiles[0].id;
      changed(); emit({ type: 'layout', data: layout() });
    },
    failNextOwner(device: string, code: ProfileError['code'] = 'os_apply_failed') { failures.set(device, { code, device, message: `Fixture ${code}` }); },
    resolveUnknown(requestId: string) {
      const op = operations.get(requestId) ?? failure('stale_state', 'Unknown request ID');
      op.owners.filter(o => o.phase === 'unknown').forEach(o => { o.phase = 'failed'; o.error = { code: 'os_apply_failed', message: 'Fixture status resolved: not applied', device: o.device }; });
      op.status = op.history_required ? 'partial' : 'failed'; op.error = op.owners.find(o => o.error)?.error ?? null;
      changed(); operationEvent();
    },
  };
}
