import { useSyncExternalStore } from 'react';
import { toast } from 'oink-ui';
import { api, call, onAgentEvent, shell } from './client';
import { toFailure } from './transport';
import { problemText, tr } from '../i18n';
import type * as T from './types';
import type { DeviceCardsView, DeviceControlCompleted, DeviceControlReply } from './cards';
import { pairingApi } from './pairing';
import type { PairingEvent, PairingSnapshot, NearbySnapshot } from './pairing';
import type { LayoutProfilesState } from './layoutProfiles';
import { createCardSignalsStore } from './cardSignals';
export type DurablePairingEvent = Extract<PairingEvent,{type:'pairing_failed'|'pairing_completed'}>;

export type { BlockedDevice, BlockedDevicesSnapshot } from './cards';
import type { BlockedDevicesSnapshot } from './cards';
export type ProductEvent = T.ApiEvent | PairingEvent
  | {type:'update_status_changed';data:import('./update').UpdateStatus}
  | {type:'dnd_export_failed';data:'cancelled'|'timeout'|'failed'}
  | {type:'dnd_preparation_failed';data:string}
  | {type:'exit_for_update'}
  | { type: 'device_cards'; data: DeviceCardsView }
  | { type: 'device_card_signals'; data: import('./cards').DeviceCardSignals }
  | import('./cards').BlockedDevicesChanged
  | { type: 'report_status_changed'; data: import('../settings/model').ReportStatus }
  | { type: 'ui_preferences'; data: import('../settings/model').LocalPreferences }
  | DeviceControlCompleted;
export interface AgentState {
  boot: T.BootInfo | null;
  update: import('./update').UpdateStatus | null;
  localPreferences: import('../settings/model').LocalPreferences | null;
  report: import('../settings/model').ReportStatus | null;
  cards: DeviceCardsView | null;
  blocked: BlockedDevicesSnapshot | null;
  overview: T.Overview | null;
  layout: T.Layout | null;
  network: T.NetworkInfo | null;
  nearby: NearbySnapshot | null;
  pairings: PairingSnapshot | null;
  pairingEvents: DurablePairingEvent[];
  input: T.InputSettings | null;
  launch: T.LaunchAtLoginStatus | null;
  diagnostics: T.DiagnosticRecorderStatus | null;
  clipboard: T.ClipboardSettings | null;
  audio: T.AudioStatus | null;
  microphonePlugin: T.MicrophonePluginStatus | null;
  permissions: T.PermissionItem[];
  fileProvider: T.FileProviderStatus | null;
  receivedFiles:T.ReceivedFilesUsage | null;
  wallpapers: Record<string, string>;
  layoutProfiles: LayoutProfilesState | null;
  /** @deprecated Legacy bottom-centred backend notice; new code must use the in-app oink-ui toast instead. */
}
const initial = (): AgentState => ({ launch: null, diagnostics: null, update: null, localPreferences: null, report:null, boot: null, cards: null, blocked: null, overview: null, layout: null, network: null, nearby: null, pairings: null, pairingEvents: [], input: null, clipboard: null, audio: null, microphonePlugin:null, permissions: [], fileProvider: null, receivedFiles:null, wallpapers: {}, layoutProfiles: null });

/** A stream belongs to one subscription; a late query cannot select another stream. */
export function acceptSnapshot<T extends { stream: string; revision: string }>(current: T | null, incoming: T): T | null {
  if (!/^\d+$/.test(incoming.revision)) return current;
  if (!current) return incoming;
  if (current.stream !== incoming.stream) return current;
  return BigInt(incoming.revision) > BigInt(current.revision) ? incoming : current;
}

/** Profile state is ordered by backend epoch/sequence, never by arrival time. A new
 * epoch (backend restart) replaces the old stream wholesale: every view is complete. */
export function newerProfiles(current: LayoutProfilesState | null, incoming: LayoutProfilesState): boolean {
  if (!current || current.epoch !== incoming.epoch) return true;
  return /^\d+$/.test(incoming.sequence) && BigInt(incoming.sequence) > BigInt(current.sequence);
}

function requestProblem(code: string) {
  if(code==='volume_unsupported')return;
  // A failed request is a receipt, not a retained statement about backend health.
  // The native shell owns the authenticated subscription and exits on its loss.
  toast.danger({ id: `request:${code}`, title: problemText(code), duration: 4200 });
}
function requestFailure(error: unknown) { requestProblem(toFailure(error).problem.code); }
/** Legacy bottom-centred notices are no longer shown; their codes go to the UI log. */
function logNotice(code: string) { void shell.logNotice(code).catch(() => {}); }

export function createAgentStore() {
  const cardSignals = createCardSignalsStore();
  let state = initial();
  let generation = 0;
  const listeners = new Set<() => void>();
  const eventListeners = new Set<(event:ProductEvent)=>void | boolean>();
  let initialPairEvents:PairingEvent[] = [], initialNearbyEvents:PairingEvent[] = [];
  const revisions = new Map<keyof AgentState, number>();
  const set = (patch: Partial<AgentState>) => { state = { ...state, ...patch }; listeners.forEach(l => l()); };
  const adopt = <K extends keyof AgentState>(key: K, value: AgentState[K]) => {
    revisions.set(key, (revisions.get(key) ?? 0) + 1);
    set({ [key]: value });
  };
  const receive = (event: ProductEvent, publish = true) => {
    if (event.type === 'device_card_signals') { cardSignals.receive(event.data); return; }
    // A live control may own retries and final error presentation for its exact
    // request. Other refusals still use the global receipt/toast path.
    let handled = false;
    if (publish) eventListeners.forEach(listener => { if (listener(event) === true) handled = true; });
    switch (event.type) {
      case 'no_shared_microphone': return void toast.info({title:tr('当前没有设备共享麦克风')});
      case 'device_cards': {
        const cards=acceptSnapshot(state.cards,event.data);
        if (cards) cardSignals.seed(cards);
        revisions.set('cards',(revisions.get('cards')??0)+1);
        return set({cards});
      }
      case 'blocked_devices': return adopt('blocked', acceptSnapshot(state.blocked, event.data.snapshot));
      case 'device_control_completed': {
        const result=event.data.result;
        const failed=result.outcome==='failed'||result.outcome==='refused';
        if(!handled&&failed&&result.reason!=='volume_unsupported'&&result.reason!=='outcome_unknown')logNotice(result.reason);
        return;
      }
      case 'update_status_changed': return adopt('update',event.data);
      // Drag failures stay in the backend log; the window shows no notice for them.
      case 'dnd_export_failed': case 'dnd_preparation_failed': return;
      // Native shell owns activation intent; replaying this as shell.open would
      // turn a backend notification into a second user-activation request.
      case 'show_window': return;
      case 'wallpapers': return void query('wallpapers', async()=>Object.fromEntries((await api.wallpapers(false)).map(w=>[`${w.key.device}:${w.key.display}`,w.image])), true).catch(requestFailure);
      case 'report_status_changed':
        if(state.report && event.data.consent.generation < state.report.consent.generation)return;
        return adopt('report',event.data);
      case 'ui_preferences': return adopt('localPreferences', event.data);
      case 'overview': return adopt('overview', event.data);
      case 'layout': return adopt('layout', event.data);
      case 'nearby_snapshot': {
        if(state.nearby && BigInt(event.data.revision)<BigInt(state.nearby.revision))return;
        adopt('nearby',event.data);const events=initialNearbyEvents;initialNearbyEvents=[];events.forEach(event=>receive(event,false));return;
      }
      case 'nearby_changed': {
        if (!state.nearby) { initialNearbyEvents.push(event); return; }
        if (BigInt(event.data.revision) < BigInt(state.nearby.revision)) return;
        const rows=state.nearby.rows.filter(row=>!event.data.removed.includes(row.device) && !event.data.upserts.some(next=>next.device===row.device));
        return adopt('nearby',{revision:event.data.revision,rows:[...rows,...event.data.upserts]});
      }
      case 'pairing_changed': {
        if (!state.pairings) { initialPairEvents.push(event); return; }
        if (BigInt(event.data.revision) < BigInt(state.pairings.revision)) return;
        return adopt('pairings',{...state.pairings,revision:event.data.revision,sessions:[...state.pairings.sessions.filter(p=>p.id!==event.data.view.id),event.data.view]});
      }
      case 'pairing_requests_changed': {
        if (!state.pairings) { initialPairEvents.push(event); return; }
        if (BigInt(event.data.revision) < BigInt(state.pairings.revision)) return;
        return adopt('pairings',{...state.pairings,revision:event.data.revision,requests:event.data.requests});
      }
      case 'pairing_completed': case 'pairing_failed':
        return set({pairingEvents:[...state.pairingEvents.filter(e=>e.data.event_id!==event.data.event_id),event]});
      case 'launch_at_login': return adopt('launch',event.data);
      case 'diagnostics_status': return adopt('diagnostics',event.data);
      case 'input_settings': return adopt('input', event.data);
      case 'clipboard_settings': return adopt('clipboard', event.data);
      case 'audio': return adopt('audio', event.data);
      case 'permissions': return adopt('permissions', event.data);
      case 'received_files_usage': return adopt('receivedFiles',event.data);
      case 'file_provider_status': return adopt('fileProvider', event.data);
      case 'emergency_stop': return logNotice('emergency_stop');
      case 'layout_profiles': return adoptProfiles(event.data);
      case 'layout_profile_operation': return adoptProfiles(event.data.state);

    }
  };
  const adoptProfiles = (next: LayoutProfilesState) => { if (newerProfiles(state.layoutProfiles, next)) adopt('layoutProfiles', next); };
  const query = async <K extends keyof AgentState>(key: K, fetch: () => Promise<AgentState[K]>, snapshot = false) => {
    const sent = revisions.get(key) ?? 0, session = generation;
    let value: AgentState[K];
    try { value = await fetch(); }
    catch (error) {
      // Apply the same lifetime/revision rule to failed replies as to successful
      // ones. A retired request must not report an outage after fresh facts won.
      // Command callers also use query() to adopt their returned state. Their
      // failure remains a receipt even if another command published newer facts.
      if (snapshot && (generation !== session || (revisions.get(key) ?? 0) !== sent)) return;
      throw error;
    }
    if (generation !== session) return;
    if(key==='cards'&&value){receive({type:'device_cards',data:value as DeviceCardsView},false);return;}
    if(key==='blocked'&&value){adopt('blocked',acceptSnapshot(state.blocked,value as BlockedDevicesSnapshot));return;}
    if(key==='layoutProfiles'&&value){adoptProfiles(value as LayoutProfilesState);return;}
    if (key==='pairings' && !state.pairings) {
      adopt(key,value); const events=initialPairEvents; initialPairEvents=[]; events.forEach(event=>receive(event,false)); return;
    }
    if (key==='nearby' && !state.nearby) {
      adopt(key,value); const events=initialNearbyEvents; initialNearbyEvents=[]; events.forEach(event=>receive(event,false)); return;
    }
    if ((revisions.get(key) ?? 0) === sent) adopt(key, value);
  };
  return {
    cardSignals,
    get: () => state,
    subscribe: (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener); }; },
    receive, query, adopt, adoptProfiles,
    refresh: <K extends keyof AgentState>(key: K, fetch: () => Promise<AgentState[K]>) => query(key, fetch, true),
    controlReply: (reply: DeviceControlReply) => { if (reply.type === 'device_control_completed') receive(reply); },
    onEvent: (listener:(event:ProductEvent)=>void | boolean) => {eventListeners.add(listener);return()=>{eventListeners.delete(listener);};},
    consumePairingEvent: (id:string) => set({pairingEvents:state.pairingEvents.filter(event=>event.data.event_id!==id)}),
    reset: () => { generation++; revisions.clear(); cardSignals.reset(); initialPairEvents=[]; initialNearbyEvents=[]; set(initial()); },
    /** @deprecated Logs the legacy bottom-centred notice; nothing is shown. New code must use the in-app oink-ui toast instead. */
    fail: (reason: string) => {
      if(reason==='volume_unsupported'||reason==='outcome_unknown')return;
      if (reason === 'unavailable' || reason === 'local_service_unavailable') {
        // Legacy catch-all callers have no evidence of IPC loss. Only the typed
        // transport problem may say that the local service could not be reached.
        requestProblem(reason === 'unavailable' ? 'backend_failed' : reason);
        return;
      }
      if(reason)logNotice(reason);
    },
  };
}
export const agent = createAgentStore();
let stopped: (() => void) | null = null;
let starting: Promise<void> | null = null;
let microphoneWarmup: ReturnType<typeof setTimeout> | null = null;
let warmupGeneration=0;
async function warmMicrophonePlugin(generation:number):Promise<void> {
  await agent.refresh('microphonePlugin', api.microphonePluginStatus);
  if(generation===warmupGeneration&&agent.get().microphonePlugin?.refreshing) {
    microphoneWarmup=setTimeout(()=>{void warmMicrophonePlugin(generation).catch(requestFailure);},750);
  }
}
export function start(view = 'main'): Promise<void> {
  if (starting) return starting;
  starting = (async () => {
    try {
      stopped = await onAgentEvent(agent.receive);
      const boot = await shell.bootInfo();
      agent.adopt('boot', boot);
      if(view!=='main'&&view!=='panel'){await agent.refresh('localPreferences',()=>call<import('../settings/model').LocalPreferences>('ui_preferences'));return;}
      const settle = (queries: Promise<void>[]) => Promise.allSettled(queries.map(p => p.catch(requestFailure)));
      // These snapshots do not decide the initial route or its layout. In
      // particular, wallpaper extraction must not hold onboarding at alpha zero.
      void settle([
        warmMicrophonePlugin(warmupGeneration), agent.refresh('diagnostics', api.diagnosticsStatus), agent.refresh('update', api.updateStatus), agent.refresh('blocked', api.blockedDevices),
        agent.refresh('network', api.networkInfo), agent.refresh('nearby', pairingApi.nearby), agent.refresh('pairings', pairingApi.pairings),
        agent.refresh('wallpapers', async () => Object.fromEntries((await api.wallpapers(true)).map(w => [`${w.key.device}:${w.key.display}`, w.image]))),
      ]);
      await settle([
        agent.refresh('launch', api.launchAtLoginStatus), agent.refresh('localPreferences', () => call<import('../settings/model').LocalPreferences>('ui_preferences')), agent.refresh('report',()=>call<import('../settings/model').ReportStatus>('report_status')), agent.refresh('cards', api.deviceCards),
        agent.refresh('overview', api.overview), agent.refresh('layout', api.layout),
        agent.refresh('input', api.inputSettings),
        agent.refresh('clipboard', api.clipboardSettings), agent.refresh('audio', api.audioStatus),
        agent.refresh('permissions', api.permissions), agent.refresh('fileProvider', api.fileProviderStatus),
        agent.refresh('layoutProfiles', api.layoutProfiles),
      ]);

    } catch (error) { requestFailure(error); }
  })();
  return starting;
}
export function stop() { warmupGeneration++; if(microphoneWarmup!==null)clearTimeout(microphoneWarmup); microphoneWarmup=null; stopped?.(); stopped = null; starting = null; agent.reset(); }
export function useAgent(): AgentState { return useSyncExternalStore(agent.subscribe, agent.get); }
export function adoptLayout(layout: T.Layout) { agent.adopt('layout', layout); }
