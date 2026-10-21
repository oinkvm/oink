

export type Hex = string;
export type DeviceKey = Hex;
export type GroupKey = Hex;

export interface Point { x: number; y: number }
export interface Size { w: number; h: number }
export interface Rect { x: number; y: number; w: number; h: number }

export interface ScreenKey { device: DeviceKey; display: string }

export type { ApiError } from './cards';

export type Os = "macos" | "windows" | "linux" | "other";

export interface DeviceSummary {
  id: DeviceKey;
  fingerprint: string;
  name: string;
  os: Os;
  os_version: string;
  model: string | null;
  app_version: string;
}

export type ServiceState = "starting" | "running" | "paused" | "degraded" | "stopped";

export interface GroupSummary { id: GroupKey; member_count: number; online_count: number }

export interface Focus {
  device: DeviceKey;
  device_name: string;
  display: string | null;
  display_name: string | null;
  is_local: boolean;
}

export type AlertLevel = "info" | "warning" | "error";
export type AlertKind =
  | { type: "capture_unavailable" }
  | { type: "display_info_unavailable"; screen: ScreenKey }
  | { type: "permission_missing"; permissions: PermissionItem[] }
  | { type: "secure_input"; app: string | null }
  | { type: "peer_offline"; device: DeviceKey }
  | { type: "new_screen"; screen: ScreenKey }
  | { type: "emergency_stop"; device: DeviceKey }
  | { type: "other" }

  | { type: "group_state_unreadable" };

export interface Alert { id: string; level: AlertLevel; kind: AlertKind; problem: import('./cards').UserFacingProblem | null; at_ms: number }

export interface PauseStatus {
  paused: boolean;
  scope: "local";
  revision: number;
  author: DeviceKey | null;
  author_name: string | null;
  emergency: boolean;
  reason: "none" | "local_user" | "remote_user" | "emergency" | "input_backlog" | "unknown";
  capture_detached: boolean;
  settling: boolean;
}

export interface Overview {
  this_device: DeviceSummary;

  group: GroupSummary | null;
  service: ServiceState;
  sharing_enabled: boolean;
  pause: PauseStatus;
  focus_lock: DeviceSummary | null;
  focus: Focus | null;
  onboarding_done: boolean;
  alerts: Alert[];
}

export type PeerState =
  | { type: "online" }
  | { type: "connecting" }
  | { type: "offline"; last_seen_ms: number | null }
  | { type: "address_stale"; last_address: string };

export type LinkKind =
  | { type: "direct" }
  | { type: "manual_ip" }
  | { type: "relay"; via: DeviceKey; via_name: string };

export interface LinkInfo { kind: LinkKind; remote_address: string | null; rtt_ms: number; loss_pct: number }

export interface FeelProfile {
  key_repeat_delay_ms: number;
  key_repeat_interval_ms: number;
  wheel_lines_per_notch: number | null;
  natural_scroll_mouse: boolean;
  natural_scroll_touchpad: boolean;
}

export type SharingState = "not_sharing" | "establishing" | "active";

export interface Member {
  device: DeviceSummary;
  is_self: boolean;

  state: PeerState;

  reachable: boolean;

  facts_fresh: boolean;
  input_transform: InputTransform | null;
  keyboard_disabled: boolean | null;
  mouse_mode: "shared" | "local" | null;
  keyboard_mode: "shared" | "local" | null;
  screen_disabled: boolean | null;
  screen_local_input: boolean | null;
  sharing: SharingState;
  link: LinkInfo | null;
  added_by: string | null;
  added_at_ms: number;
  display_count: number;
  feel: FeelProfile | null;
}

export interface GroupView { id: GroupKey | null; members: Member[] }

export type InterfaceKind = "ethernet" | "wifi" | "vpn" | "other";
export interface LocalAddress { ip: string; interface: string; kind: InterfaceKind }
export interface NetworkInfo { addresses: LocalAddress[]; port: number }

export interface DisplayInfo {
  key: string;
  name: string;
  bounds: Rect;
  pixel_width: number;
  pixel_height: number;
  reference_extent: Size | null;
  rotation_deg: number;
  is_primary: boolean;
  is_builtin: boolean;
  is_virtual: boolean;
}

export type SizeSource = "reported" | "estimated" | "manual";

export interface Screen {
  key: ScreenKey;
  device_name: string;
  os: Os;
  display: DisplayInfo;

  size: Size;
  size_source: SizeSource;

  origin: Point;
  gain: number;
  remote_gain?: number;
  hidden: boolean;
  device_disabled: boolean;

  online: boolean;
}

export interface Placement { device: DeviceKey; origin: Point; scale: number }

export interface LayoutGeometry { screens: Screen[]; bounds: Rect }
export type LayoutSource = "default" | "manual";
export interface Layout {
  revision: number;
  source: LayoutSource;
  geometry: LayoutGeometry;
}

export type KeyPreset = "win_feel" | "mac_feel" | "custom" | "none";
export interface KeyMapping { from: number; to: number }
export interface KeyboardMapping { preset: KeyPreset; table: KeyMapping[]; caps_tap_hold?: { tap: number; hold: number } | null }
export interface TargetTransform { reverse_wheel: boolean; keyboard: KeyboardMapping }
export interface InputLocalExclusion { scope: string; target: DeviceKey | null; mapping: KeyMapping; reason: string }
export interface InputTransform { local_exclusions?: InputLocalExclusion[]; macos: TargetTransform; windows: TargetTransform; overrides: { target: DeviceKey; reverse_wheel: boolean | null; keyboard: KeyboardMapping | null }[] }
export interface ScrollSettings { speed: number }
export type PointerReference = { type: "last_screen" } | { type: "fixed"; display: string };
export type PointerScaling = "physical" | "proportional";
export interface PointerSettings { reference: PointerReference; scaling: PointerScaling; drag_across_devices: boolean }
export type HotkeyAction = "lock_focus" | "return_pointer_home" | "toggle_sharing" | "open_window" | "toggle_game_mode" | "pause_all";
export interface Hotkey { action: HotkeyAction; keys: string[]; enabled: boolean }
export type EventFrequency = "hz125" | "unlimited";
export interface KeySupport { macos: number[]; windows: number[] }
export interface InputSettings {
  mac_caps_switches_input_source: boolean | null;
  key_support: KeySupport;
  /** Local normal pointer-motion rate; persisted by the settings owner. */
  event_frequency: EventFrequency;
  accept_contacts: boolean;
  contacts_injection_available: boolean;
  mac_scroll_feel: boolean;
  /** Windows capture-local opt-in; false keeps other apps' injected input local. */
  forward_injected_input: boolean;
  emergency_hold_ms: number;
  device_gain: number;
  scroll: ScrollSettings;
  pointer: PointerSettings;
  hotkeys: Hotkey[];
  gestures_enabled: boolean;

  return_pointer_on_display_sleep: boolean;
  wake_displays_on_input: boolean;

  cursor_trail: boolean;
  cursor_trail_duration_ms: number;
  /** Group setting, stored and shared with the layout. */
  gap_travel: boolean;
}

export type CacheCategory = "clipboard_files" | "drag_files" | "provider_catalog" | "provider_materialized" | "provider_temporary" | "delivered_files";
export interface CacheUsage { categories: { category: CacheCategory; bytes: number; kept_bytes: number }[]; total_bytes: number; incomplete: boolean }
export interface CacheDevice { device: DeviceKey; usage: CacheUsage }
export interface CacheDeviceClear { device: DeviceKey; result: { removed_bytes: number; kept_bytes: number; usage: CacheUsage } }

export interface ClipboardSettings {
  receive?: boolean; send?: boolean;
  text: boolean; rich_text: boolean; images: boolean; files: boolean; custom: boolean;
}
export type ClipKind = "text" | "html" | "rtf" | "image" | "files" | "custom";
export type ClipOutcome =
  | { type: "synced" }
  | { type: "skipped"; reason: string }
  | { type: "failed"; message: string };
export interface ClipActivity { at_ms: number; from_name: string; kinds: ClipKind[]; bytes: number; outcome: ClipOutcome }

export type FileProviderState =
  | "unsupported"
  | "checking"
  | "unregistered"
  | "not_enabled"
  | "available"
  | "disconnected"
  | "error";
export interface FileProviderStatus {
  state: FileProviderState;

  needed: boolean;

  opening: boolean;

  detail: string | null;
}

export interface PeerStatus {
  device: DeviceKey;
  name: string;
  os: Os;
  state: PeerState;
  reachable: boolean;
  facts_fresh: boolean;
  sharing: SharingState;
  link: LinkInfo | null;
  rtt_history_ms: number[];
}
export interface SettingsIssue { path: string; kind: "unknown_key" | "defaulted" | "dropped_entry" }
export interface StatusView {
  settings_issues: SettingsIssue[];
  backend_lifetime: string;
  backend_uiaccess: boolean | null;
  service: ServiceState;
  version: string;
  uptime_s: number;
  focus: Focus | null;
  peers: PeerStatus[];
  alerts: Alert[];
}
export type LogLevel = "error" | "warn" | "info" | "debug" | "trace";
export interface LogLine { seq: number; at_ms: number; level: LogLevel; target: string; message: string }
export interface DiagnosticsBundle {
  path: string;
  bytes: number;
  cutoff_complete: boolean | null;
  included_device_count: number | null;
  missing_device_count: number | null;
}

export type PermissionKind = "accessibility" | "input_monitoring" | "local_network" | "firewall" | "system_service" | "audio_capture";
export type PermissionState = "Granted" | "Denied" | "NotDetermined" | "NotApplicable" | "NotGranted";
export interface PermissionItem { kind: PermissionKind; state: PermissionState; required: boolean }

export type GameMode = "auto" | "on" | "off";

export interface GameModeStatus {
  holder: DeviceKey;
  epoch: number;
  mode: GameMode;

  detected: boolean;

  active: boolean;

  cursor_hiding_cause_available: boolean | null;
}

export type AimPhase = "idle" | "countdown" | "running" | "finished";
export interface AimResult {

  native_input: boolean;

  mode_changed: boolean;
  score: number;
  clicks: number;

  accuracy: number | null;
  hits_per_sec: number;
  avg_hit_interval_ms: number | null;
  played_ms: number;
  completed: boolean;
  devices: number;
  ended_at_ms: number;
}
export interface AimStatus {
  available: boolean;
  phase: AimPhase;

  host: boolean;

  native_input: boolean;
  remaining_ms: number;
  score: number;
  clicks: number;
  devices: number;
  target_diameter: number;
  duration_ms: number;
  results: AimResult[];
}

export type ApiEvent =
  | import('./layoutProfiles').LayoutProfilesEvent
  | { type: "connection_quality"; data: ConnectionQuality }
  | { type: "audio_quality"; data: AudioQuality }
  | { type: "diagnostics_status"; data: DiagnosticRecorderStatus }
  | { type: "launch_at_login"; data: LaunchAtLoginStatus }
  | { type: "audio"; data: AudioStatus }
  | { type: "audio_stats"; data: AudioStatistics }
  | { type: "diagnostics_stats"; data: DiagnosticStatistics }
  | { type: "game_mode"; data: GameModeStatus }
  | { type: "overview"; data: Overview }
  | { type: "pause"; data: PauseStatus }
  | { type: "focus"; data: Focus | null }
  | { type: "group"; data: GroupView }



  | { type: "input_transform_edited"; data: { device: DeviceKey; error: string | null } }

  | { type: "layout"; data: Layout }
  | { type: "input_settings"; data: InputSettings }
  | { type: "cache_usage"; data: CacheDevice[] }
  | { type: "received_files_usage"; data: ReceivedFilesUsage }
  | { type: "clipboard_settings"; data: ClipboardSettings }
  | { type: "clip_activity"; data: ClipActivity }
  | { type: "file_provider_status"; data: FileProviderStatus }
  | { type: "permissions"; data: PermissionItem[] }
  | { type: "stats"; data: PeerStatus[] }
  | { type: "log"; data: LogLine }
  | { type: "emergency_stop"; data: EmergencyStop }

  | { type: "show_window" }
  | { type: "aim"; data: AimStatus }

  | { type: "wallpapers" };

export interface ScreenWallpaper {
  key: ScreenKey;

  rev: string;

  image: string;
}

export interface EmergencyStop { device: DeviceKey; device_name: string; local: boolean; at_ms: number }

export type LaunchAtLoginState =

  | "enabled"

  | "disabled"

  | "requires_approval"

  | "unavailable";

export interface LaunchAtLoginStatus {
  state: LaunchAtLoginState;
  configured: boolean;

  problem: null | "main_app_required" | "installed_app_required" | "approval_required" | "different_installation" | "invalid_run_entry" | "system_unavailable" | "unsupported_platform";
}

export interface BootInfo {
  open_update_failure?: boolean;
  build: string | null;
  /** "agent": connected to the backend; "none": backend unavailable. */
  backend: "agent" | "none";
  platform: "macos" | "windows" | "linux";
  version: string;
}

// Diagnostic recorder IPC schema (UI composition belongs to the UI agent).
export interface DiagnosticRecorderStatus {

  configured_enabled: boolean;

  suspended: boolean;

  enabled: boolean;

  recovering: boolean;
  bytes: number;
  limit_bytes: number;
  queued: number;
  dropped: number;
  errors: number;
  segments: number;
  /** Retained on-disk rows, local UTC microseconds; uncalibrated remote rows use source UTC. */
  oldest_utc_us: number | null;
  newest_utc_us: number | null;
  corrupt_segments: number;
  evicted_segments: number;
}

export type AudioDeviceId = number[];
export type AudioFormat = "pcm16_2p5ms" | "opus5ms";
export type AudioCapturePermission = "not_requested" | "unknown" | "authorized" | "denied" | "not_required";
export interface AudioSettings {
  receive_output: string | null;
  forwarding_off_reason: "local_volume_changed" | "default_output_changed" | null;
  keep_local_playback: boolean;
  capture_scope: "default_output" | "global";
  preferred_format: AudioFormat;
  /** Local speaker source only; has no effect while this device is the exit. */
  forwarding_enabled: boolean;
  /** Local speaker forwarding gain, integer 0–200; 100 is unchanged. */
  forwarding_volume_percent: number;
  forwarding_muted: boolean;
}
export interface AudioOutput { id: string; name: string; is_default: boolean }
export interface AudioDeviceOutputs { device: AudioDeviceId; outputs: AudioOutput[]; capture: string }
export interface AudioOutputLatency { output: string; reported_latency_ms: number | null }
export interface AudioReceiverReport { source: AudioDeviceId; received: number; concealed: number; late: number; buffered_ms: number }

export type AudioSuspension =
  | "connecting"
  | "group_paused"
  | "sleeping"
  | "preparing"
  | "exit_unavailable"
  | "device_unavailable"
  | "mute_release_unconfirmed";
export interface AudioDeviceState {
  phase: number;
  problem: AudioProblem | null;
  permission: AudioCapturePermission;
  source_muted: boolean;
  forwarding_enabled: boolean;
  forwarding_off_reason: AudioStatus["forwarding_off_reason"];
  forwarding_muted: boolean;
  forwarding_volume_percent: number;
  mute_release_unconfirmed: boolean;
  sent: number;
  send_dropped: number;
  outputs: AudioOutputLatency[];
  receivers: AudioReceiverReport[];
}
export interface AudioDeviceReport { device: AudioDeviceId; state: AudioDeviceState; age_ms: number }
export interface AudioPlaybackInfo {
  sample_rate: number;
  channels: number;
  requested_buffer_frames: number | null;
  legacy_queue: boolean;
  backend: unknown;
  reported_output_latency_ms: number | null;
}
export interface AudioMuteRecovery {
  outcome: "nothing_to_recover" | "restored" | "already_original" | "user_override" | "endpoint_unavailable";
  endpoint: string | null;
  crash_intent_uncertain: boolean;
}
export interface AudioCaptureInfo {
  diagnostics: unknown;
  backend: string;
  sample_rate: number;
  requested_buffer_frames: number | null;
  actual_buffer_frames: number;
  engine_period_frames: number | null;
  engine_default_period_frames: number | null;
  engine_min_period_frames: number | null;
  engine_max_period_frames: number | null;
  engine_fundamental_period_frames: number | null;
  reported_latency_frames: number | null;
  safety_offset_frames: number | null;
  physical_output_id: string | null;
  physical_output_sample_rate: number | null;
  physical_output_buffer_frames: number | null;
  physical_output_original_buffer_frames: number | null;
  physical_output_buffer_lease_changed: boolean;
  physical_output_latency_frames: number | null;
  physical_output_safety_offset_frames: number | null;
  timer_resolution_requested_ms: number | null;
  timer_resolution_request_succeeded: boolean;
  timer_resolution_error: string | null;
  timestamp_source: string;
  fallback_reason: string | null;
}
export interface AudioReceiverStats {
  received: number;
  lost: number;
  late: number;
  duplicates: number;
  rejected: number;
  decode_errors: number;
  rebufferings: number;
  queued_frames: number;
  buffered_ms: number;
  drift_ppm: number;
  frame_duration_ms: number;
  initial_target_ms: number;
  current_target_ms: number;
  min_target_ms: number;
  max_target_ms: number;
  packet_queue_ms: number;
  decoded_queue_ms: number;
  peak_buffered_ms: number;
  arrival_jitter_ms: number;
  arrival_spread_ms: number;
  sender_timed_packets: number;
  startup_hold_frames: number;
  startup_wait_ms: number;
  adaptation_hold_frames: number;
  adaptation_hold_ms: number;
  target_increases: number;
  target_decreases: number;
  gap_plc_frames: number;
  empty_queue_plc_frames: number;
  plc_ms: number;
  latency_trimmed_frames: number;
  latency_trimmed_ms: number;
  boundary_hold_samples: number;
}
export interface AudioSampleLevels {
  peak_dbfs: number | null; rms_dbfs: number | null; zero_frame_fraction: number;
  frames: number; nonfinite_samples: number;
}
export interface AudioCaptureLevels {
  window_sequence: number; window_ms: number; age_ms: number;
  before_gain: AudioSampleLevels; after_gain: AudioSampleLevels | null;
  wasapi_buffers: number | null; wasapi_silent_buffers: number | null;
}
export interface AudioClaimStamp {counter:number;owner:AudioDeviceId}
export interface AudioProblem {code:number;native_code:number|null;retry_after_ms:number|null}
export interface AudioStatus {
  /** This node's 3s Low pacing audio hold; speaker silence is excluded. */
  audio_activity: boolean;
  settings: AudioSettings;
  resolution: {kind:"unknown"}|{kind:"none";barrier:AudioClaimStamp|null}|{kind:"exit";stamp:AudioClaimStamp;available:boolean};
  devices: AudioDeviceOutputs[];
  device_states: AudioDeviceReport[];
  capture_permission: AudioCapturePermission;
  phase: number;

  suspension: AudioSuspension | null;
  forwarding_off_reason: "local_volume_changed" | "default_output_changed" | null;
  problem: AudioProblem | null;
  source_muted: boolean;
  mute_release_unconfirmed: boolean;
  mute_recovery: AudioMuteRecovery | null;
  capture_info: AudioCaptureInfo | null;
  capture_levels?: AudioCaptureLevels | null;
  capture_restart_reason: string | null;
  playback_info: AudioPlaybackInfo | null;
  sent: number;
  send_dropped: number;
  sources: [AudioDeviceId, AudioReceiverStats][];
  network_rtt_ms: number | null;
  estimated_latency_ms: number | null;
}

/** Samples owned by audio's report cadence; never replace lifecycle/settings. */
export interface AudioStatistics {
  capture_levels: AudioCaptureLevels | null;
  capture_diagnostics: unknown | null;
  playback_latency_ms: number | null;
  sent: number;
  send_dropped: number;
  sources: AudioStatus["sources"];
  network_rtt_ms: number | null;
  estimated_latency_ms: number | null;
  device_states: {
    device: AudioDeviceId; age_ms: number; sent: number; send_dropped: number;
    outputs: AudioOutputLatency[]; receivers: AudioReceiverReport[];
  }[];
}
export type DiagnosticStatistics = Pick<DiagnosticRecorderStatus,
  "bytes" | "queued" | "dropped" | "errors" | "segments" |
  "oldest_utc_us" | "newest_utc_us" | "corrupt_segments" | "evicted_segments">;

export interface LiveViews { stats: boolean; group_quality: boolean; audio_quality: boolean; diagnostics: boolean }

export interface QualitySample { at_ms: number; rtt_ms: number | null }
export interface PeerQuality {
  device: string;
  name: string;
  path: LinkKind;
  samples: QualitySample[];
  current_ms: number | null;
  p50_ms: number | null;
  p90_ms: number | null;
  p99_ms: number | null;
  max_ms: number | null;
}
export interface ConnectionQuality { revision: number; sampled_at_ms: number; peers: PeerQuality[] }

/** Desktop notification, drawn by the backend even when the UI is closed. */
export interface Notification {
  level: 'notice' | 'info' | 'debug';
  title: string;
  description?: string;
  screen?: ScreenKey;
  persistent?: boolean;
}
export interface NotificationReceipt { id: string; screen: ScreenKey | null; remote: boolean }

export interface AudioDropoutBucket { at_ms: number; interruptions: number; concealed_ns: number; missing_ns: number; late_packets: number; silence_interruptions: number; concealment_interruptions: number; discontinuities: number }
export interface AudioQualitySample { at_ms: number; buffer_ms: number | null }
export interface AudioSourceQuality {
  device: DeviceKey; name: string; samples: AudioQualitySample[];
  buffer_avg_ms: number | null;
  bitrate_kbps: number; interruptions: number; concealed_ms: number; late_packets: number; glitches: AudioDropoutBucket[];
}
export interface AudioOutputQuality { interruptions: number; missing_ms: number; glitches: AudioDropoutBucket[] }
export interface AudioQuality { revision: number; sampled_at_ms: number; sources: AudioSourceQuality[]; output: AudioOutputQuality | null }

export type ReceivedFilesCleanup = "never" | "days7" | "days30";
export interface ReceivedFilesUsage { materialized_bytes:number; incomplete:boolean; time_basis:"received"|"last_used"; cleanup:ReceivedFilesCleanup }
