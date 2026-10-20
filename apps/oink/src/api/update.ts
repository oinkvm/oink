export type UpdatePhase = 'disabled'|'idle'|'checking'|'available'|'downloading'|'verifying'|'ready'|'quiescing'|'installing'|'awaiting_reboot'|'installer_busy'|'committed'|'rolled_back'|'repair_required'|'failed';
export type UpdateBlocker = 'drag'|'promise'|'file_provider'|'clipboard_bulk'|'remote_input'|'held_input'|'router_transition'|'audio'|'microphone'|'configuration'|'arrangement'|'locked'|'suspended'|'session_changed'|'owner_missing'|'consent'|'integration_unavailable';
export type UpdateError = 'metadata'|'signature'|'expired_feed'|'replay'|'clock'|'url'|'limit'|'policy'|'os_unsupported'|'busy'|'transaction'|'repair_required'|'storage'|'not_configured'|'network';
export interface UpdateStatus {
  current_build:number;version:string;channel:'stable'|'beta';checks_enabled:boolean;
  phase:UpdatePhase;candidate:{build:number;version:string;size:number;published_at:number|null;severity:string;notes:{'zh-Hans':string;en:string}}|null;
  checked_at:number|null;next_check:number|null;deferred_until:number|null;blockers:UpdateBlocker[];progress:number|null;error:UpdateError|null;scope:'user'|'machine';can_change_policy:boolean;
}
export interface UpdatePreferences { checks_enabled:boolean;channel:'stable'|'beta' }
export type UpdateCommand = {type:'check'|'download'|'cancel'}|{type:'install'|'skip';expected_build:number}
  |{type:'set_preferences';preferences:UpdatePreferences}|{type:'defer';choice:'hour'|'tomorrow'|'next_quit'|'indefinitely'};
