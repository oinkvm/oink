export type UpdatePhase = 'disabled'|'idle'|'checking'|'available'|'downloading'|'verifying'|'waiting_for_idle'|'quiescing'|'installing'|'awaiting_reboot'|'installer_busy'|'committed'|'rolled_back'|'repair_required'|'failed';
export type UpdateBlocker = 'drag'|'promise'|'file_provider'|'clipboard_bulk'|'remote_input'|'held_input'|'router_transition'|'audio'|'microphone'|'configuration'|'arrangement'|'locked'|'suspended'|'session_changed'|'owner_missing'|'consent'|'idle_interval'|'integration_unavailable';
export type UpdateError = 'metadata'|'signature'|'expired_feed'|'replay'|'clock'|'url'|'limit'|'policy'|'os_unsupported'|'busy'|'transaction'|'repair_required'|'storage'|'not_configured'|'network';
export interface UpdateStatus {
  current_build:number;version:string;channel:'stable'|'beta';mode:'notify'|'automatic';checks_enabled:boolean;
  phase:UpdatePhase;candidate:{build:number;version:string;size:number;published_at:number|null;severity:string;notes:{'zh-Hans':string;en:string}}|null;
  checked_at:number|null;next_check:number|null;deferred_until:number|null;blockers:UpdateBlocker[];progress:number|null;error:UpdateError|null;scope:'user'|'machine';can_change_policy:boolean;
}
export type UpdateCommand = {type:'check'|'download'|'cancel'}|{type:'install';expected_build:number};
