export type UpdatePhase = 'disabled'|'idle'|'checking'|'available'|'downloading'|'verifying'|'ready'|'quiescing'|'installing'|'awaiting_reboot'|'installer_busy'|'committed'|'rolled_back'|'repair_required'|'failed';
export type UpdateBlocker = 'drag'|'promise'|'file_provider'|'clipboard_bulk'|'remote_input'|'held_input'|'router_transition'|'audio'|'microphone'|'configuration'|'arrangement'|'locked'|'suspended'|'session_changed'|'owner_missing'|'consent'|'integration_unavailable';
export type { UpdateError } from './cards';
import type { UpdateError } from './cards';
export interface UpdateStatus {
  current_build:number;version:string;checks_enabled:boolean;
  phase:UpdatePhase;candidate:{local_package?:boolean;build:number;version:string;size:number;published_at:number|null;severity:string;notes:(Record<string,string>&{en:string})|null}|null;
  checked_at:number|null;next_check:number|null;deferred_until:number|null;blockers:UpdateBlocker[];progress:number|null;error:UpdateError|null;scope:'user'|'machine';can_change_policy:boolean;
}
export interface UpdatePreferences { checks_enabled:boolean }
export type UpdateCommand = {type:'open_local';path:string}| {type:'check'|'download'|'cancel'}|{type:'install'|'skip';expected_build:number}
  |{type:'set_preferences';preferences:UpdatePreferences}|{type:'defer';choice:'hour'|'tomorrow'|'next_quit'|'indefinitely'};
