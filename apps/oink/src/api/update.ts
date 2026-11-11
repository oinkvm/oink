export type UpdatePhase = 'disabled'|'idle'|'checking'|'available'|'downloading'|'verifying'|'ready'|'quiescing'|'installing'|'awaiting_reboot'|'installer_busy'|'committed'|'rolled_back'|'repair_required'|'failed';
export type UpdateBlocker = 'drag'|'promise'|'file_provider'|'clipboard_bulk'|'remote_input'|'held_input'|'router_transition'|'audio'|'microphone'|'configuration'|'arrangement'|'locked'|'suspended'|'session_changed'|'owner_missing'|'consent'|'integration_unavailable';
export type { UpdateError } from './cards';
import type { UpdateError } from './cards';
export type UpdateChannel = 'stable'|'beta';
/** How the feed's rules route this installation to the candidate. */
export type UpdateKind = 'normal'|'critical'|'manual';
export type UpdateNotes = Record<string,string>&{en:string};
export interface UpdateStatus {
  current_build:number;version:string;checks_enabled:boolean;
  /** The channel in effect; update_channel is the stored choice (null follows the package). */
  channel:UpdateChannel;update_channel:UpdateChannel|null;install_after_download:boolean;
  phase:UpdatePhase;candidate:{local_package?:boolean;build:number;version:string;size:number;published_at:number|null;severity:string;notes:UpdateNotes|null;
    kind:UpdateKind;/** Shown in red before the notes of a critical update. */notice:UpdateNotes|null}|null;
  checked_at:number|null;next_check:number|null;deferred_until:number|null;blockers:UpdateBlocker[];progress:number|null;error:UpdateError|null;
  /** The build whose download failed; the failure is not shown for another build. */
  failed_build:number|null;
  /** The critical update whose dialog already opened by itself. */
  critical_prompted_build:number|null;
  scope:'user'|'machine';can_change_policy:boolean;
}
export interface UpdatePreferences { checks_enabled:boolean;update_channel:UpdateChannel|null;install_after_download:boolean }
/** Preferences as stored, with one change applied. */
export const preferencesWith=(status:UpdateStatus,patch:Partial<UpdatePreferences>):UpdatePreferences=>({
  checks_enabled:status.checks_enabled,update_channel:status.update_channel,install_after_download:status.install_after_download,...patch,
});
export type UpdateCommand = {type:'open_local';path:string}| {type:'check'|'download'|'cancel'}|{type:'install'|'skip'|'prompted';expected_build:number}
  |{type:'set_preferences';preferences:UpdatePreferences}|{type:'defer';choice:'hour'|'tomorrow'|'next_quit'|'indefinitely'};
