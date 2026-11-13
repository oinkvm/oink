
import { toFailure, transport } from "./transport";
import type * as T from "./types";
import { createLayoutProfilesApi } from './layoutProfiles';

export { ApiFailure } from "./transport";

export type AutoUnlockFailure = 'password_incorrect' | 'windows_hello_only' | 'login_ui_not_ready' | 'unknown';
/** Why the latest 同步解锁 request was skipped on this device; never a failure. */
export type AutoUnlockSyncSkip = 'lock_screen' | 'display_wake' | 'password_field' | 'source_locked' | 'local_typing' | 'source_disconnected';
export type AutoUnlockStatus = { enabled: boolean; failure: AutoUnlockFailure | null; windows_hello_only: boolean; sync_last?: { kind: AutoUnlockSyncSkip; at_ms: number } | null };

export async function call<R>(method: string, params?: Record<string, unknown>): Promise<R> {
  try {
    return await transport.call<R>(params ? { method, params } : { method });
  } catch (e) {
    throw toFailure(e);
  }
}

function requireLayout(layout:T.Layout):T.Layout {
  if (!Array.isArray(layout?.geometry?.screens) || layout.geometry.screens.some(s => !s.origin || !s.size || ![s.origin.x,s.origin.y,s.size.w,s.size.h].every(Number.isFinite))) {
    throw new Error('Unsupported layout response');
  }
  return layout;
}
function requireInput(settings:T.InputSettings):T.InputSettings {
  if (typeof settings.accept_contacts !== 'boolean' || !Number.isFinite(settings.device_gain) || !Number.isInteger(settings.emergency_hold_ms)) throw new Error('Unsupported input settings response');
  return settings;
}
export const api = {
  autoUnlockStatus: () => call<AutoUnlockStatus>('auto_unlock_status'),
  saveAutoUnlock: (password:string) => call<null>('save_auto_unlock', {password}),
  disableAutoUnlock: () => call<null>('disable_auto_unlock'),
  ...createLayoutProfilesApi(call),
  audioQuality: () => call<T.AudioQuality>("audio_quality"),
  connectionQuality: () => call<T.ConnectionQuality>("connection_quality"),
  memberAddresses: (device: string) => call<string[]>('member_addresses', {device}),
  /** The latest background measurement (once a minute); reading never probes. */
  memberRoutes: (device: string) => call<T.MemberRoutes>('member_routes', {device}),
  /** Applied at once; `false` when the choice is not possible now. */
  setMemberRoute: (device: string, selection: T.RouteSelection) => call<boolean>('set_member_route', {device, selection}),
  setMemberAddress: (device: string,address: string) => call<null>('set_member_address', {device,address}),
  moveDeviceOrder: (device: string, placement: import('../devices/reorder').Placement) => call<null>('move_device_order', { device, placement }),
  updateStatus: () => call<import('./update').UpdateStatus>('query_update_status'),
  update: (command: import('./update').UpdateCommand) => call<import('./update').UpdateStatus>('update', { command }),
  lockFocusTo: (device: string) => call<null>('lock_focus_to', { device }),
  setForwardingControls: (requestId: string, versionToken: string, volumePercent: number, muted: boolean, deferPersistence = false) => call<import('./cards').DeviceControlReply>('set_forwarding_controls', { requestId, versionToken, volumePercent, muted, ...(deferPersistence ? { deferPersistence } : {}) }),
  deviceCards: () => call<import('./cards').DeviceCardsView>('device_cards'),
  // Phones: start only where start_allowed (Android privileged); "切回本地" is a card audio mode.
  mobileAudioState: (device: string) => call<T.MobileAudioState | null>('mobile_audio_state', { device }),
  startMobileAudioEndpoint: (target: T.MobileAudioTarget) => call<null>('start_mobile_audio_endpoint', { target }),
  blockedDevices: () => call<import('./store').BlockedDevicesSnapshot>('blocked_devices'),
  block: (device: string) => call<import('./cards').BlockedDeviceReply>('block', { operation_id: crypto.randomUUID(), device }),
  unblock: (device: string) => call<import('./cards').BlockedDeviceReply>('unblock', { operation_id: crypto.randomUUID(), device }),
  invokeCard: (requestId: string, token: string) => call<import('./cards').DeviceControlReply>('invoke_device_card_action', { requestId, token }),
  setExitVolume: (requestId: string, versionToken: string, volumePercent: number, muted: boolean) => call<import('./cards').DeviceControlReply>('set_exit_volume', { requestId, versionToken, volumePercent, muted }),

  setInputTransform: (device: T.DeviceKey, settings: T.InputTransform) => call<null>("set_input_transform", { device, settings }),
  overview: () => call<T.Overview>("overview"),
  setLocalPaused: (requestId:string,paused:boolean) => call<import('./cards').DeviceControlReply>('set_local_paused',{requestId,paused}),
  setDevicePaused: (requestId:string,token:string,paused:boolean) => call<import('./cards').DeviceControlReply>('set_device_paused',{requestId,token,paused}),
  pauseAll: (requestId:string) => call<import('./cards').PauseAllAccepted>('pause_all',{requestId}),

  group: () => call<T.GroupView>("group"),
  removeMember: (device: T.DeviceKey) => call<null>("remove_member", { device }),
  leaveGroup: () => call<null>("leave_group"),
  networkInfo: () => call<T.NetworkInfo>("network_info"),

  previewLayoutProjection: (gesture: number, placements: T.Placement[], affected: T.ScreenKey[], ended: boolean, arrangement?: import('./layoutProfiles').ArrangementSnapshot) =>
    call<null>('preview_layout_projection', { gesture, placements, affected, ended, arrangement }),
  layout: () => call<T.Layout>("layout").then(requireLayout),
  setScreenRemoteGain: (screen: T.ScreenKey, gain: number) => call<null>("set_screen_remote_gain", { screen, gain }),

  wallpapers: (refresh: boolean) => call<T.ScreenWallpaper[]>("wallpapers", { refresh }),


  inputSettings: () => call<T.InputSettings>("input_settings").then(requireInput),
  setInputSettings: (settings: Partial<T.InputSettings>) => call<null>("set_input_settings", { settings }),

  receivedFilesUsage: () => call<T.ReceivedFilesUsage>("received_files_usage"),
  setReceivedFilesCleanup: (cleanup:T.ReceivedFilesCleanup) => call<null>("set_received_files_cleanup",{cleanup}),
  openReceivedFiles: () => call<null>("open_received_files"),
  cacheUsage: (device: T.DeviceKey | null = null) => call<T.CacheDevice[]>("cache_usage", { device }),
  clearCache: (device: T.DeviceKey | null = null) => call<T.CacheDeviceClear>("clear_cache", { device }),
  clipboardSettings: () => call<T.ClipboardSettings>("clipboard_settings"),
  setClipboardSettings: (settings: Partial<T.ClipboardSettings>) => call<null>("set_clipboard_settings", { settings }),
  clipboardActivity: () => call<T.ClipActivity[]>("clipboard_activity"),

  fileProviderStatus: () => call<T.FileProviderStatus>("file_provider_status"),

  openFileProviderEnable: () => call<null>("open_file_provider_enable"),

  setMicrophoneInput: (input: string | null) => call<null>("set_microphone_input", {input}),
  audioStatus: () => call<T.AudioStatus>("audio_status"),
  // Call only after the user confirms the restart dialog.
  restartSystem: () => call<null>("restart_system"),
  microphonePluginStatus: () => call<T.MicrophonePluginStatus>("microphone_plugin_status"),
  microphonePluginAction: (action:'install'|'uninstall') => call<T.MicrophonePluginStatus>("microphone_plugin_action",{action}),

  setAudioSettings: (settings: Partial<T.AudioSettings>) => call<null>("set_audio_settings", { settings }),

  status: () => call<T.StatusView>("status"),
  logs: (after: number | null, limit: number) => call<T.LogLine[]>("logs", { after, limit }),
  /** Saved coverage and cutoff summary; names appear only in diagnosticsStatus. */
  exportDiagnostics: (lastMinutes: number | null = 30): Promise<T.DiagnosticsBundle> => call<T.DiagnosticsBundle>("export_diagnostics", { last_minutes: lastMinutes }),
  diagnosticsStatus: (): Promise<T.DiagnosticRecorderStatus> => call<T.DiagnosticRecorderStatus>("diagnostics_status"),
  setDiagnosticsEnabled: (enabled: boolean) => call<null>("set_diagnostics_enabled", { enabled }),

  permissions: () => call<T.PermissionItem[]>("permissions"),
  requestPermission: (kind: T.PermissionKind) => call<null>("request_permission", { kind }),
  openPermissionSettings: (kind: T.PermissionKind) => call<null>("open_permission_settings", { kind }),
  completeOnboarding: () => call<null>("complete_onboarding"),

  reopenOink: () => call<null>("reopen_oink"),

  resetGroupState: () => call<null>("reset_group_state"),

  launchAtLoginStatus: () => call<T.LaunchAtLoginStatus>("launch_at_login_status"),

  setLaunchAtLogin: (enabled: boolean) => call<T.LaunchAtLoginStatus>("set_launch_at_login", { enabled }),

  openLaunchAtLoginSettings: () => call<null>("open_launch_at_login_settings"),

};

export const shell = {
  uninstall: (messages: {title:string;failure:string;done:string;processes:string;privacy:string;data:string;trash:string}) => transport.shell<void>('shell_uninstall', {messages}),
  open: (view: 'main' | 'panel' | 'about' | 'quit') => transport.shell<void>('shell_open', { view }),
  close: () => transport.shell<void>('shell_close'),
  ready: () => transport.shell<void>('shell_ready', { generation: (window as Window & { __OINK_WINDOW_GENERATION__?: number }).__OINK_WINDOW_GENERATION__ ?? 0 }),
  titlebarCenter: () => transport.shell<number | null>('shell_titlebar_center'),
  quit: () => transport.shell<void>('shell_quit'),
  resizePanel: (width: number, height: number) => transport.shell<void>('shell_resize_panel', { width, height }),
  showPanel: () => transport.shell<void>('shell_show_panel'),
  openUrl: (url: string) => transport.shell<void>('shell_open_url', { url }),
  attribution: (path: string) => transport.shell<string>('read_attribution', { path }),
  setLanguage: (language: import('../i18n').LanguageChoice) => transport.shell<void>('shell_set_language', {language}),
  setLogLevel: (level: string) => transport.shell<void>('shell_set_log_level', {level}),
  logNotice: (code: string) => transport.shell<void>('frontend_notice', {code}),
  setStatusIcon: (visible: boolean) => transport.shell<void>('shell_set_status_icon', {visible}),
  openCacheFolder: () => transport.shell<void>('open_cache_folder'),
  openDiagnosticsFolder: () => transport.shell<void>('open_diagnostics_folder'),

  liveViews: (views: T.LiveViews) => transport.shell<void>("live_views", { views }),
  bootInfo: () => transport.shell<T.BootInfo>("boot_info"),

};

export const onAgentEvent = (handler: (e: import('./store').ProductEvent) => void) => transport.onEvent(handler);
