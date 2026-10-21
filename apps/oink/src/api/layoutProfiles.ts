import type { DeviceKey, Placement, Point, ScreenKey, Size } from './types';

/** Opaque tokens/revisions are strings: Rust u64 values must not pass through JS numbers. */
export type Token = string;
export type ProfileName = { kind: 'default' } | { kind: 'custom'; value: string };
export interface Disabled { screen: ScreenKey; disabled: boolean }
export interface DeviceSharing { device: DeviceKey; shared: boolean }
export interface NativeState {
  device: DeviceKey;
  token: Token;
  primary: ScreenKey;
  screens: { screen: ScreenKey; origin: Point; size: Size }[];
}
export interface Topology {
  gap_travel: boolean;
  overlap_tolerance: number;
  minimum_portal: number;
  portal_snap: number;
  walls: boolean;
  endpoint_reach: number;
}
export interface ArrangementSnapshot {
  placements: Placement[];
  disabled: Disabled[];
  screen_shared?: DeviceSharing[];
  native: NativeState[];
  topology: Topology;
  active_profile_id: string;
}
export interface ProfileSummary {
  id: string;
  name: ProfileName;
  content_revision: Token;
  last_used_ms: string;
  exact_match: boolean;
  active: boolean;
}
export interface DeviceArrangementState {
  device: DeviceKey;
  sync_system_arrangement: boolean;
  preference_token: Token;
  native_token: Token | null;
  availability: 'ready' | 'offline' | 'unsupported' | 'unavailable';
  can_edit_screens: boolean;
}
export type AdjustmentCode = 'device_added' | 'device_removed' | 'screen_added' | 'screen_removed'
  | 'native_changed' | 'anchor_changed' | 'anchor_displaced' | 'overlap_repaired'
  | 'native_snapped' | 'owner_offline';
export interface Adjustment { code: AdjustmentCode; device: DeviceKey; screen?: ScreenKey }
export type ProfileErrorCode = 'stale_state' | 'profile_not_found' | 'profile_limit' | 'invalid_name'
  | 'invalid_geometry' | 'disconnected' | 'no_valid_placement' | 'size_limit'
  | 'facts_unavailable' | 'owner_offline' | 'unsupported' | 'setting_disabled'
  | 'busy' | 'not_member' | 'persistence_failed' | 'os_apply_failed'
  | 'native_changed' | 'outcome_unknown' | 'duplicate_name' | 'last_profile';
export interface ProfileError {
  code: ProfileErrorCode;
  message: string;
  device?: DeviceKey;
  screen?: ScreenKey;
}
export interface OwnerProgress {
  device: DeviceKey;
  phase: 'applying' | 'applied' | 'failed' | 'skipped_offline' | 'unknown';
  error: ProfileError | null;
}
export interface LayoutOperation {
  id: string;
  kind: 'load' | 'edit' | 'reset' | 'replay';
  status: 'running' | 'applied' | 'partial' | 'failed' | 'outcome_unknown';
  before: ArrangementSnapshot;
  after: ArrangementSnapshot | null;
  native_restore_devices: DeviceKey[];
  adjustments: Adjustment[];
  owners: OwnerProgress[];
  history_required: boolean;
  error: ProfileError | null;
}
export interface LayoutProfilesState {
  epoch: string;
  sequence: string;
  token: Token;
  catalog_token: Token;
  readiness: 'initializing' | 'ready' | 'syncing' | 'error';
  profiles: ProfileSummary[];
  active_profile_id: string | null;
  create_enabled: boolean;
  snapshot: ArrangementSnapshot | null;
  devices: DeviceArrangementState[];
  pending: OwnerProgress[];
  operation: LayoutOperation | null;
}
export interface Mutation { request_id: string; expected_token: Token }
export interface CatalogMutation { request_id: string; expected_catalog_token: Token }
export interface OperationReply { operation_id: string; state: LayoutProfilesState }
export interface OperationQuery { operation: LayoutOperation; state: LayoutProfilesState }
export interface PreviewRequest {
  sequence: string;
  expected_token: Token;
  device: DeviceKey;
  moved_screen: ScreenKey;
  desired_canvas_origin: Point;
}
export interface PreviewReply {
  sequence: string;
  native: NativeState;
  placements: Placement[];
  plan_token: Token;
  adjustments: Adjustment[];
}
export interface CommitRequest extends Mutation {
  placements: Placement[];
  native_plan_tokens: Token[];
  disabled: Disabled[];
  screen_shared?: DeviceSharing[];
}
export interface ReplayRequest extends Mutation {
  from: ArrangementSnapshot;
  target: ArrangementSnapshot;
  native_restore_devices: DeviceKey[];
  direction: 'undo' | 'redo';
  original_kind: 'load' | 'edit' | 'reset';
}
export interface SettingRequest {
  request_id: string;
  device: DeviceKey;
  expected_preference_token: Token;
  enabled: boolean;
}
export type LayoutProfilesEvent =
  | { type: 'layout_profiles'; data: LayoutProfilesState }
  | { type: 'layout_profile_operation'; data: {
    epoch: string; sequence: string; operation: LayoutOperation; state: LayoutProfilesState;
  } };

/** Business refusals are result envelopes, so existing IPC ApiError handling stays intact. */
export interface ProfileErrorReply { type: 'profile_error'; error: ProfileError }
export class LayoutProfileFailure extends Error {
  constructor(readonly problem: ProfileError) {
    super(problem.message);
    this.name = 'LayoutProfileFailure';
  }
}
export type ProfileCall = <R>(method: string, params?: Record<string, unknown>) => Promise<R>;

/** Injection supports UI fixtures without changing the application's native transport. */
export function createLayoutProfilesApi(call: ProfileCall) {
  async function invoke<R>(method: string, params?: object): Promise<R> {
    const result = await call<R | ProfileErrorReply>(method, params as Record<string, unknown> | undefined);
    if (result && typeof result === 'object' && 'type' in result && result.type === 'profile_error') {
      throw new LayoutProfileFailure((result as ProfileErrorReply).error);
    }
    return result as R;
  }
  return {
    layoutProfiles: () => invoke<LayoutProfilesState>('layout_profiles'),
    createLayoutProfile: (request: Mutation & { name: string }) => invoke<LayoutProfilesState>('create_layout_profile', request),
    renameLayoutProfile: (request: CatalogMutation & { profile_id: string; name: string }) =>
      invoke<LayoutProfilesState>('rename_layout_profile', request),
    deleteLayoutProfile: (request: CatalogMutation & { profile_id: string }) =>
      invoke<LayoutProfilesState>('delete_layout_profile', request),
    loadLayoutProfile: (request: Mutation & { profile_id: string }) =>
      invoke<OperationReply>('load_layout_profile', request),
    layoutProfileOperation: (request_id: string) => invoke<OperationQuery>('layout_profile_operation', { request_id }),
    setSyncSystemArrangement: (request: SettingRequest) => invoke<LayoutProfilesState>('set_sync_system_arrangement', request),
    previewSystemArrangement: (request: PreviewRequest) => invoke<PreviewReply>('preview_system_arrangement', request),
    commitArrangement: (request: CommitRequest) => invoke<OperationReply>('commit_arrangement', request),
    resetArrangement: (request: Mutation) => invoke<OperationReply>('reset_arrangement', request),
    replayArrangement: (request: ReplayRequest) => invoke<OperationReply>('replay_arrangement', request),
  };
}
