// Authenticated local IPC contract. IDs are full stable public-key hex strings.
// Subscribe before querying; accept only increasing revisions in the current stream.
// A new subscription retires the previous stream and all its outstanding replies.
// Tokens bind owner Boot + option revision, independent of card/RTT revisions.
// Accepted means dispatched. Only owner facts and completion confirm the result.
// This module contains no transport, mock state, or UI business logic.

export type DeviceKey = string; // existing full public-key hex
export type GroupId = string; // group identifier, never a shared cryptographic key
export type DecimalU64 = string;
export type UtcMs = number; // integer, 0..Number.MAX_SAFE_INTEGER
export type SharingMode = "shared" | "local" | "disabled";
export type CardOs = "macos" | "windows" | "linux" | "other";
export type CardLogo = "macos" | "windows10" | "linux" | "generic";
export type OfflineKind = "disconnected" | "connecting" | "address_stale" | "protocol_incompatible";
export type Signal =
  | { kind: "measured"; rttMs: number; bars: 1 | 2 | 3;
      colour: "green" | "yellow" | "red" }
  | { kind: "unknown"; rttMs: null; bars: 0; colour: "grey" }
  | { kind: "offline"; rttMs: null; bars: 0; colour: "grey" };
export type CardConnection =
  | { kind: "self" }
  | { kind: "online"; signal: Extract<Signal, { kind: "measured" | "unknown" }> }
  | { kind: "offline"; reason: OfflineKind;
      signal: Extract<Signal, { kind: "offline" }> };

// permission_required includes a failed preflight with no evidence of a user refusal.
export type UserFacingCode = "unspecified" | "permission_required" | "permission_denied"
  | "capture_unavailable" | "injection_unavailable" | "secure_input"
  | "device_unavailable" | "cleanup_failed" | "unsupported" | "backend_failed"
  | "link_lost" | "timeout" | "storage_full" | "invalid_value"
  | "state_conflict" | "local_service_unavailable" | "invalid_address" | "not_member" | "persistence_failed" | "network_stopped" | "unrecognized";
export interface UserFacingProblem {
  code: UserFacingCode;
  unknownCode: number | null; // preserves an unrecognized wire u32
  nativeCode: number | null;
  retryAfterMs: number | null;
}
export type ResourceFaultCode =
  | "none" | "permission_required" | "permission_denied"
  | "capture_unavailable" | "injection_unavailable" | "secure_input"
  | "device_unavailable" | "cleanup_failed" | "unsupported"
  | "backend_failed" | "unrecognized";
export interface ResourceFault {
  code: ResourceFaultCode;
  unknownCode: number | null; // u32 only for unrecognized
  problem: UserFacingProblem | null; // UI translates
}
export type CardCondition =
  | "ready" | "local_paused" | "sleeping" | "starting"
  | "waiting_for_target" | "screen_disabled_fallback"
  | "faulted" | "unsupported" | "state_unknown" | "state_stale";
export type CardActionUnavailable =
  | "offline" | "unsupported" | "state_unknown" | "state_stale"
  | "remote_fault" | "not_member" | "busy";
export type CardAction =
  | { kind: "invoke"; command: "set_screen_mode" | "set_local_option"
      | "clear_audio_exit" | "make_audio_exit" | "set_game_mode"
      | "set_device_paused"; token: string }
  | { kind: "open_permissions" } // self fault only; UI-local navigation
  | { kind: "unavailable"; reason: CardActionUnavailable };
export interface ResourceCard<M extends string, I extends string = M> {
  action: CardAction; // all five resources, self and remote
  configuredMode: M | null; // this device owner's applied preference, excluding pause
  icon: I | "fault" | "unknown";
  fault: ResourceFault | null; // null = no authoritative report; none != null
  condition: CardCondition;
}
export interface CursorCard {
  icon: "local" | "remote" | "locked" | "unknown";
  focusDevice: DeviceKey | null;
  focusName: string | null; // resolved by backend
  lockedDevice: DeviceKey | null;
  context: "group" | "independent" | "local_paused" | "unknown";
}
export interface SpeakerCard extends ResourceCard<
  "unused" | "receiver" | "source", "unused" | "exit" | "forwarding"
> {
  makeExitAction: CardAction;
  forwardingControlsToken: string | null; // owner15 Boot/revision fence
  forwardingEnabled: boolean | null; // this row's source switch
  localOutput: { volumePercent: number /* 0..100 */; muted: boolean } | null;
  activity: "idle" | "playing" | "forwarding" | "sound_sleep"
    | "preparing" | "waiting_for_exit" | "paused" | "sleeping"
    | "failed" | "unknown";
  receiverDevice: DeviceKey | null; // self if receiving; source's observed destination
  receiverName: string | null;
  activeSources: number | null; // 0..16, receiver-authored
  receiveReady: boolean | null;
  forwarding: {
    volumePercent: number; // integer 0..200, control position, default 100
    volumeBars: 0 | 1 | 2 | 3 | 4;
    muted: boolean; // forwarding mute, independent of gain/OS mute
  } | null;
}
export interface ClipboardCard extends ResourceCard<SharingMode> {}
export interface DeviceCardActions {
  screenShared: CardAction;
  screenLocal: CardAction;
  mouseShared: CardAction;
  mouseLocal: CardAction;
  mouseDisabled?: CardAction;
  keyboardShared: CardAction;
  keyboardLocal: CardAction;
  keyboardDisabled: CardAction;
  keyboardEnabled: CardAction;
  clipboardShared: CardAction;
  clipboardLocal: CardAction;
  forwardingEnabled: CardAction;
  forwardingDisabled: CardAction;
}
export interface DeviceCardStatus {
  accessibilityGranted?: boolean | null; // unknown/legacy/not applicable when absent or null
  actions: DeviceCardActions;
  cursor: CursorCard;
  screen: ResourceCard<SharingMode>;
  mouse: ResourceCard<SharingMode>;
  keyboard: ResourceCard<SharingMode, SharingMode | "disabled"> & {
    disabled: boolean | null;
  };
  speaker: SpeakerCard;
  clipboard: ClipboardCard;
  game: { preference: "auto" | "on" | "off" | null; active: boolean | null;
          detected: boolean | null; actions: Record<"auto" | "on" | "off", CardAction> };
}
export interface DeviceCard {
  device: DeviceKey;
  name: string;
  os: CardOs;
  logo: CardLogo;
  isSelf: boolean;
  section: "this_device" | "online" | "offline";
  connection: CardConnection;
  lastSeenMs: UtcMs | null; // ALL offline reasons; null for self/online/never seen
  status: DeviceCardStatus | null; // null on every offline row
  pause: LocalPauseCard | null; // null offline/unknown; observation, never command
}
export interface ExitVolumeCard {
  receiverDevice: DeviceKey;
  receiverName: string; // the resolved unique exit
  outputName: string | null;
  volumePercent: number | null; // actual system volume, integer 0..100
  muted: boolean | null; // system mute, not forwarding mute
  condition: "ready" | "offline" | "state_unknown" | "unsupported"
    | "unavailable" | "permission_denied" | "backend_failed";
  versionToken: string | null; // exact exit claim + Boot/output generation/revision
  canSet: boolean;
  refusal: CardActionUnavailable | "output_unavailable"
    | "permission_denied" | null;
}
export interface LocalPauseCard {
  action: CardAction; // toggle the current owner pause; no Resume all
  paused: boolean;
  reason: "none" | "local_user" | "remote_user" | "emergency" | "input_backlog" | "unknown";
  revision: DecimalU64;
  canResume: boolean; // exact live owner Boot/pause revision available, self or remote
}
export interface EmergencyDeliveryCard {
  eventToken: string; // opaque local identity; one event across all recipients
  recipients: { device: DeviceKey;
    state: "queued" | "confirmed" | "unconfirmed" | "unreachable";
    cleanupPending: boolean | null }[];
}
export interface DeviceCardsView {
  schema: 1;
  stream: string; // opaque random token for this backend/projection incarnation
  revision: DecimalU64; // monotonic within stream, starts at 1
  group: GroupId | null; // null in unreadable-group recovery
  generatedAtMs: UtcMs;
  exitVolume: ExitVolumeCard | null; // current unique exit, unavailable identity retained
  emergencyDelivery: EmergencyDeliveryCard | null; // local latest event coverage
  cards: DeviceCard[]; // already grouped and ordered, includes self
}
export type DeviceCardsChanged = { type: "device_cards"; data: DeviceCardsView };

export type DeviceControlFailure =
  | "offline" | "unsupported" | "stale_boot" | "stale_revision"
  | "not_member" | "resource_fault" | "invalid_value" | "busy"
  | "persistence_failed" | "apply_failed" | "link_lost" | "outcome_unknown"
  | "not_exit" | "route_changed" | "output_changed" | "output_unavailable"
  | "volume_unsupported" | "permission_denied" | "superseded"
  | "unsupported_option" | "state_conflict" | "unrecognized";
export type DeviceControlResult =
  | { outcome: "applied" | "no_change" }
  | { outcome: "refused" | "failed"; reason: DeviceControlFailure;
      unknownCode: number | null };
export type DeviceControlRequest =
  | { method: "set_forwarding_controls"; params: { requestId: string; versionToken: string;
      volumePercent: number; muted: boolean; deferPersistence?: boolean } }
  | { method: "invoke_device_card_action"; params: { requestId: string; token: string } }
  | { method: "set_exit_volume"; params: { requestId: string; versionToken: string;
      volumePercent: number; muted: boolean } };
export type DeviceControlAccepted = { type: "device_control_accepted"; data: { requestId: string } };
export type DeviceControlCompleted = {
  type: "device_control_completed";
  data: { requestId: string;
  result: DeviceControlResult;
  };
};

export type DeviceControlReply = DeviceControlAccepted | DeviceControlCompleted;

export interface BlockedDevice {
  device: DeviceKey;
  name: string | null;
  os: string | null;
  blocked_at_ms: UtcMs;
}
export interface BlockedDevicesSnapshot {
  stream: string;
  revision: DecimalU64;
  entries: BlockedDevice[];
}
export type BlockedDevicesChanged = { type: "blocked_devices"; data: { snapshot: BlockedDevicesSnapshot } };
export type BlockedDeviceRequest =
  | { method: "blocked_devices" }
  | { method: "block"; params: { operation_id: string; device: DeviceKey } }
  | { method: "unblock"; params: { operation_id: string; device: DeviceKey } };

export interface ApiError {
  code: "unavailable" | "not_found" | "invalid_argument" | "invalid_state"
    | "network" | "unsupported" | "internal" | "invalid_address" | "not_member" | "persistence_failed" | "network_stopped";
  problem: UserFacingProblem;
}

// blocked_devices returns the snapshot; mutations return the correlated edit.
export interface BlockEdited {
  operation_id: string;
  changed: boolean;
  snapshot: BlockedDevicesSnapshot;
}
export interface BlockListError {
  code: number; // 0 InvalidIdentity, 1 LocalStorageFailed, 2 LimitExceeded, 3 LocalServiceUnavailable
  limit: number | null;
}
export type BlockedDeviceReply = BlockEdited | BlockListError;

export interface UiPreferences {
  notification_level: "disabled" | "minimal" | "default" | "verbose";
  ui_language: "auto" | "zh-Hans" | "en" | "ja" | "ko";
  theme_mode: "system" | "light" | "dark";
  theme_accent: { light: string; dark: string }; // validated opaque #RRGGBB
  show_status_icon: boolean;
  confirm_quit: boolean;
  log_level: "error" | "warn" | "info" | "debug" | "trace";
  log_max_mb: number; // positive u32, decimal megabytes
}
export type UiPreferencesRequest =
  | { method: "ui_preferences" }
  | { method: "set_ui_preferences"; params: { settings: Partial<Omit<UiPreferences,"theme_accent">> & {theme_accent?: Partial<UiPreferences["theme_accent"]>} } };
export type UiPreferencesReply = UiPreferences;
export type UiPreferencesChanged = { type: "ui_preferences"; data: UiPreferences };

export type PauseRequest =
  | { method: "set_local_paused"; params: { requestId: string; paused: boolean } }
  | { method: "set_device_paused"; params: { requestId: string; token: string; paused: boolean } }
  | { method: "pause_all"; params: { requestId: string } };
export interface PauseAllAccepted { requestId: string; eventToken: string }
