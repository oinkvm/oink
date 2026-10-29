import { describe, expect, it } from 'vitest';
import { createLayoutProfilesFixture, sameProfileContent } from './layoutProfiles.fixture';
import type { ProductEvent } from './store';

describe('layout profile fixture contract', () => {
  it('starts with a translated default and auto-saves manual edits into the current profile', async () => {
    const f = createLayoutProfilesFixture();
    expect(f.state().profiles.map(p => p.name)).toEqual([{ kind: 'default' }]);
    expect(f.state().create_enabled).toBe(true);
    const defaultId = f.state().active_profile_id!;
    const moved = structuredClone(f.state().snapshot!.placements);
    moved[0].origin.x -= .5;
    await f.api.commitArrangement({ request_id: 'edit', expected_token: f.state().token, placements: moved, native_plan_tokens: [], disabled: [] });
    await f.flush();
    const edited = f.state().snapshot!;
    await f.api.createLayoutProfile({ request_id: 'work', expected_token: f.state().token, name: 'Work' });
    const workId = f.state().active_profile_id!;
    // A replaced live layout (an automatic change) does not save into Work.
    const elsewhere = structuredClone(f.state().snapshot!);
    elsewhere.placements[0].origin.x += 3;
    f.replaceCurrent(elsewhere);
    await f.api.loadLayoutProfile({ request_id: 'back', expected_token: f.state().token, profile_id: defaultId });
    await f.flush();
    expect(sameProfileContent(f.state().snapshot!, edited)).toBe(true);
    await f.api.loadLayoutProfile({ request_id: 'work-again', expected_token: f.state().token, profile_id: workId });
    await f.flush();
    expect(f.state().snapshot!.placements[0]).toEqual(edited.placements[0]);
  });

  it('switches once and replays a switch with its selection, an edit without', async () => {
    const f = createLayoutProfilesFixture();
    const original = f.state().snapshot!;
    await f.api.createLayoutProfile({ request_id: 'as', expected_token: f.state().token, name: 'Work' });
    const moved = structuredClone(f.state().snapshot!.placements);
    moved[0].origin.x -= 1;
    await f.api.commitArrangement({ request_id: 'edit', expected_token: f.state().token, placements: moved, native_plan_tokens: [], disabled: [{ screen: original.disabled[0].screen, disabled: true }] });
    await f.flush();
    const before = f.state().snapshot!;
    const accepted = await f.api.loadLayoutProfile({ request_id: 'load', expected_token: f.state().token, profile_id: original.active_profile_id });
    expect(accepted.state.operation?.status).toBe('running');
    await f.flush();
    const result = await f.api.layoutProfileOperation('load');
    expect(result.operation.status).toBe('applied');
    expect(result.operation.history_required).toBe(true);
    expect(sameProfileContent(result.operation.after!, original)).toBe(true);
    await f.api.replayArrangement({ request_id: 'undo', expected_token: f.state().token,
      direction: 'undo', from: result.operation.after!, target: before, native_restore_devices: [], original_kind: 'load' });
    await f.flush();
    expect(sameProfileContent(f.state().snapshot!, before)).toBe(true);
    expect(f.state().snapshot!.active_profile_id).toBe(before.active_profile_id);
    await f.api.replayArrangement({ request_id: 'redo', expected_token: f.state().token,
      direction: 'redo', from: before, target: result.operation.after!, native_restore_devices: [], original_kind: 'load' });
    await f.flush();
    expect(sameProfileContent(f.state().snapshot!, original)).toBe(true);
    expect(f.state().active_profile_id).toBe(original.active_profile_id);
    // An edit replay keeps whichever profile is current, even with an old active ID in the target.
    await f.api.replayArrangement({ request_id: 'edit-undo', expected_token: f.state().token,
      direction: 'undo', from: f.state().snapshot!, target: { ...before, active_profile_id: before.active_profile_id }, native_restore_devices: [], original_kind: 'edit' });
    await f.flush();
    expect(f.state().active_profile_id).toBe(original.active_profile_id);
  });

  it('resets through one published operation and replays it without native writes', async () => {
    const f = createLayoutProfilesFixture();
    const edited = structuredClone(f.state().snapshot!);
    edited.placements[0].origin = { x: -10, y: 5 };
    edited.disabled[0].disabled = true;
    f.replaceCurrent(edited);
    const before = f.state().snapshot!;
    const request = { request_id: 'reset', expected_token: f.state().token };
    const accepted = await f.api.resetArrangement(request);
    expect(accepted.state.operation?.status).toBe('running');
    expect(accepted.state.operation?.after).toBeNull();
    await f.flush();
    const { operation, state } = await f.api.layoutProfileOperation('reset');
    expect(operation.kind).toBe('reset');
    expect(operation.status).toBe('applied');
    expect(operation.history_required).toBe(true);
    expect(operation.before).toEqual(before);
    expect(operation.after).toEqual(state.snapshot);
    expect(operation.after!.placements[0].origin).toEqual({ x: 0, y: 0 });
    expect(operation.after!.disabled).toEqual(before.disabled);
    expect(operation.after!.active_profile_id).toBe(before.active_profile_id);
    expect(operation.native_restore_devices).toEqual([]);
    expect(operation.owners).toEqual([]);
    const sequence = f.state().sequence;
    await f.api.resetArrangement(request);
    expect(f.state().sequence).toBe(sequence);
    await f.api.replayArrangement({ request_id: 'undo-reset', expected_token: f.state().token,
      from: operation.after!, target: operation.before, native_restore_devices: [], direction: 'undo', original_kind: 'reset' });
    await f.flush();
    expect(sameProfileContent(f.state().snapshot!, before)).toBe(true);
  });

  it('does not queue offline visibility on a switch', async () => {
    const f = createLayoutProfilesFixture();
    const initial = f.state(), current = structuredClone(initial.snapshot!);
    current.disabled[0].disabled = true;
    f.replaceCurrent(current);
    f.setOnline(initial.devices[0].device, false);
    await f.api.loadLayoutProfile({ request_id: 'offline-load', expected_token: f.state().token, profile_id: initial.active_profile_id! });
    await f.flush();
    expect(f.state().snapshot!.disabled[0].disabled).toBe(true);
    expect(f.state().operation?.adjustments.filter(a => a.code === 'owner_offline')).toHaveLength(2);
    f.setOnline(initial.devices[0].device, true);
    expect(f.state().snapshot!.disabled[0].disabled).toBe(true);
  });

  it('orders by most recent use only, never by matching, and touches recency on switch', async () => {
    const f = createLayoutProfilesFixture({ now: () => 1 });
    const defaultId = f.state().active_profile_id!;
    const device = f.state().devices[0].device;
    f.setOnline(device, false);
    await f.api.createLayoutProfile({ request_id: 'offline', expected_token: f.state().token, name: 'Offline' });
    const offlineId = f.state().active_profile_id!;
    f.setOnline(device, true);
    expect(f.state().profiles.map(p => p.id)).toEqual([offlineId, defaultId]);
    expect(f.state().profiles.map(p => p.exact_match)).toEqual([true, true]);
    await f.api.createLayoutProfile({ request_id: 'online', expected_token: f.state().token, name: 'Online' });
    const onlineId = f.state().active_profile_id!;
    expect(f.state().profiles.map(p => p.id)).toEqual([onlineId, offlineId, defaultId]);
    await f.api.loadLayoutProfile({ request_id: 'touch', expected_token: f.state().token, profile_id: defaultId });
    await f.flush();
    expect(f.state().profiles.map(p => p.id)).toEqual([defaultId, onlineId, offlineId]);
  });

  it('enforces sixteen profiles, stale tokens and 32-character unique names', async () => {
    const f = createLayoutProfilesFixture();
    const old = f.state().token;
    await expect(f.api.createLayoutProfile({ request_id: 'dup-default', expected_token: f.state().token, name: ' default LAYOUT ' }))
      .rejects.toMatchObject({ problem: { code: 'duplicate_name' } });
    await expect(f.api.createLayoutProfile({ request_id: 'long', expected_token: f.state().token, name: 'x'.repeat(33) }))
      .rejects.toMatchObject({ problem: { code: 'invalid_name' } });
    await f.api.createLayoutProfile({ request_id: 'cjk', expected_token: f.state().token, name: '中'.repeat(32) });
    for (let i = 0; i < 14; i++) await f.api.createLayoutProfile({ request_id: `new-${i}`, expected_token: f.state().token, name: `Profile ${i}` });
    expect(f.state().profiles).toHaveLength(16);
    expect(f.state().create_enabled).toBe(false);
    await expect(f.api.createLayoutProfile({ request_id: 'seventeenth', expected_token: f.state().token, name: 'Overflow' }))
      .rejects.toMatchObject({ problem: { code: 'profile_limit' } });
    await expect(f.api.loadLayoutProfile({ request_id: 'stale', expected_token: old, profile_id: f.state().active_profile_id! })).rejects.toMatchObject({ problem: { code: 'stale_state' } });
    await expect(f.api.renameLayoutProfile({ request_id: 'rename', expected_catalog_token: f.state().catalog_token,
      profile_id: f.state().active_profile_id!, name: '   ' })).rejects.toMatchObject({ problem: { code: 'invalid_name' } });
    await expect(f.api.renameLayoutProfile({ request_id: 'rename-dup', expected_catalog_token: f.state().catalog_token,
      profile_id: f.state().active_profile_id!, name: 'PROFILE 0' })).rejects.toMatchObject({ problem: { code: 'duplicate_name' } });
  });

  it('renames without a recency touch, refuses the last delete and selects the next MRU without a load', async () => {
    const f = createLayoutProfilesFixture({ now: () => 1 });
    const defaultId = f.state().active_profile_id!;
    await f.api.renameLayoutProfile({ request_id: 'rename', expected_catalog_token: f.state().catalog_token, profile_id: defaultId, name: '  Desk  ' });
    expect(f.state().profiles[0].name).toEqual({ kind: 'custom', value: 'Desk' });
    await expect(f.api.deleteLayoutProfile({ request_id: 'last', expected_catalog_token: f.state().catalog_token, profile_id: defaultId }))
      .rejects.toMatchObject({ problem: { code: 'last_profile' } });
    await f.api.createLayoutProfile({ request_id: 'a', expected_token: f.state().token, name: 'A' });
    const a = f.state().active_profile_id!;
    await f.api.createLayoutProfile({ request_id: 'b', expected_token: f.state().token, name: 'B' });
    const b = f.state().active_profile_id!, live = f.state().snapshot!, recency = f.state().profiles.map(p => p.last_used_ms);
    await f.api.renameLayoutProfile({ request_id: 'rename-a', expected_catalog_token: f.state().catalog_token, profile_id: a, name: 'A2' });
    expect(f.state().profiles.map(p => p.last_used_ms)).toEqual(recency);
    await f.api.deleteLayoutProfile({ request_id: 'delete-b', expected_catalog_token: f.state().catalog_token, profile_id: b });
    expect(f.state().active_profile_id).toBe(a);
    expect(f.state().operation).toBeNull();
    expect(sameProfileContent(live, f.state().snapshot!)).toBe(true);
  });

  it('keeps successful owners on a partial Load and never compensates them', async () => {
    const f = createLayoutProfilesFixture();
    const initial = f.state();
    const edited = structuredClone(initial.snapshot!);
    edited.disabled.forEach(b => { b.disabled = true; });
    f.replaceCurrent(edited);
    f.failNextOwner(initial.devices[0].device);
    const request = { request_id: 'partial', expected_token: f.state().token, profile_id: initial.active_profile_id! };
    await f.api.loadLayoutProfile(request);
    await f.flush();
    const result = (await f.api.layoutProfileOperation('partial')).operation;
    expect(result.status).toBe('partial');
    expect(result.history_required).toBe(true);
    expect(result.after!.disabled.filter(b => b.screen.device === initial.devices[0].device).every(b => b.disabled)).toBe(true);
    expect(result.after!.disabled.filter(b => b.screen.device === initial.devices[1].device).every(b => !b.disabled)).toBe(true);
    const sequence = f.state().sequence;
    await f.api.loadLayoutProfile(request);
    expect(f.state().sequence).toBe(sequence); // identical ID is a receipt lookup, not another write
  });

  it('resolves unknown outcomes by querying the original operation', async () => {
    const f = createLayoutProfilesFixture();
    f.failNextOwner(f.state().devices[0].device, 'outcome_unknown');
    await f.api.loadLayoutProfile({ request_id: 'unknown', expected_token: f.state().token, profile_id: f.state().active_profile_id! });
    await f.flush();
    expect((await f.api.layoutProfileOperation('unknown')).operation.status).toBe('outcome_unknown');
    f.resolveUnknown('unknown');
    expect((await f.api.layoutProfileOperation('unknown')).operation.status).toBe('failed');
  });

  it('provides a read-only connected native preview and records native replay eligibility', async () => {
    const f = createLayoutProfilesFixture();
    const d = f.state().devices[0], moved = f.state().snapshot!.native[0].screens[1].screen;
    await expect(f.api.previewSystemArrangement({ sequence: '1', expected_token: f.state().token, device: d.device,
      moved_screen: moved, desired_canvas_origin: { x: 0, y: -.6 } })).rejects.toMatchObject({ problem: { code: 'setting_disabled' } });
    await f.api.setSyncSystemArrangement({ request_id: 'enable', device: d.device, expected_preference_token: d.preference_token, enabled: true });
    const before = f.state().snapshot!, beforeSequence = f.state().sequence;
    const preview = await f.api.previewSystemArrangement({ sequence: '2', expected_token: f.state().token, device: d.device,
      moved_screen: moved, desired_canvas_origin: { x: 0, y: -.6 } });
    expect(f.state().sequence).toBe(beforeSequence);
    expect(preview.native.screens[1].origin).toEqual({ x: 0, y: -600 });
    await f.api.commitArrangement({ request_id: 'native-edit', expected_token: f.state().token, placements: preview.placements,
      native_plan_tokens: [preview.plan_token], disabled: [] });
    await f.flush();
    expect(f.state().operation?.native_restore_devices).toEqual([d.device]);
    expect(sameProfileContent(before, f.state().snapshot!)).toBe(false);
    await f.api.replayArrangement({ request_id: 'native-undo', expected_token: f.state().token, from: f.state().snapshot!,
      target: before, native_restore_devices: [d.device], direction: 'undo', original_kind: 'edit' });
    await f.flush();
    expect(sameProfileContent(before, f.state().snapshot!)).toBe(true);
  });

  it('emits final actual state and monotonic sequences, and rejects reused IDs with different payloads', async () => {
    const f = createLayoutProfilesFixture();
    const events: ProductEvent[] = [];
    const unsubscribe = await f.transport.onEvent(e => events.push(e));
    const req = { request_id: 'load', expected_token: f.state().token, profile_id: f.state().active_profile_id! };
    await f.api.loadLayoutProfile(req); await f.flush();
    const profileEvents = events.filter(e => e.type === 'layout_profiles');
    expect(profileEvents.length).toBeGreaterThan(1);
    expect(profileEvents.every((e, i) => !i || BigInt(e.data.sequence) > BigInt(profileEvents[i - 1].data.sequence))).toBe(true);
    const last = events.filter(e => e.type === 'layout_profile_operation').at(-1)!;
    expect(last.data.operation.status).toBe('applied');
    expect(last.data.state.snapshot).toEqual(last.data.operation.after);
    await expect(f.api.loadLayoutProfile({ ...req, profile_id: 'different' })).rejects.toMatchObject({ problem: { code: 'stale_state' } });
    unsubscribe();
  });
});
