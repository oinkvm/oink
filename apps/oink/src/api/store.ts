import { useSyncExternalStore } from 'react';
import { api, ApiFailure, call, onAgentEvent, shell } from './client';
import type * as T from './types';
import type { DeviceCardsView, DeviceControlCompleted, DeviceControlReply } from './cards';
import { pairingApi } from './pairing';
import type { PairingEvent, PairingSnapshot, NearbySnapshot } from './pairing';
export type DurablePairingEvent = Extract<PairingEvent,{type:'pairing_failed'|'pairing_completed'}>;

export type { BlockedDevice, BlockedDevicesSnapshot } from './cards';
import type { BlockedDevicesSnapshot } from './cards';
export interface PendingControl { requestId: string; device: string; resource: string; desired: string | number | boolean; confirmed?: boolean }
export type ProductEvent = T.ApiEvent | PairingEvent
  | {type:'update_status_changed';data:import('./update').UpdateStatus}
  | {type:'dnd_export_failed';data:'cancelled'|'timeout'|'failed'}
  | {type:'dnd_preparation_failed';data:string}
  | {type:'exit_for_update'}
  | { type: 'device_cards'; data: DeviceCardsView }
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
  permissions: T.PermissionItem[];
  fileProvider: T.FileProviderStatus | null;
  wallpapers: Record<string, string>;
  pending: PendingControl[];
  error: string | null;
}
const initial = (): AgentState => ({ launch: null, diagnostics: null, update: null, localPreferences: null, report:null, boot: null, cards: null, blocked: null, overview: null, layout: null, network: null, nearby: null, pairings: null, pairingEvents: [], input: null, clipboard: null, audio: null, permissions: [], fileProvider: null, wallpapers: {}, pending: [], error: null });

/** A stream belongs to one subscription; a late query cannot select another stream. */
export function acceptSnapshot<T extends { stream: string; revision: string }>(current: T | null, incoming: T): T | null {
  if (!/^\d+$/.test(incoming.revision)) return current;
  if (!current) return incoming;
  if (current.stream !== incoming.stream) return current;
  return BigInt(incoming.revision) > BigInt(current.revision) ? incoming : current;
}

export function createAgentStore() {
  let state = initial();
  let generation = 0;
  const listeners = new Set<() => void>();
  const eventListeners = new Set<(event:ProductEvent)=>void>();
  let initialPairEvents:PairingEvent[] = [], initialNearbyEvents:PairingEvent[] = [];
  const revisions = new Map<keyof AgentState, number>();
  const set = (patch: Partial<AgentState>) => { state = { ...state, ...patch }; listeners.forEach(l => l()); };
  const adopt = <K extends keyof AgentState>(key: K, value: AgentState[K]) => {
    revisions.set(key, (revisions.get(key) ?? 0) + 1);
    set({ [key]: value });
  };
  // Option receipts and focus gossip use different streams. A successful
  // screen command stays prospective until both observations match, in either
  // arrival order. Refusals still restore the current owner facts immediately.
  const reconcileScreens = (pending: PendingControl[], cards: DeviceCardsView | null) => pending.filter(p=>{
    if(p.resource!=='screen'||!p.confirmed)return true;
    const status=cards?.cards.find(card=>card.device===p.device)?.status;
    return !status || status.cursor.icon==='locked' || status.screen.configuredMode!==(p.desired==='shared'?'shared':'local');
  });
  const receive = (event: ProductEvent, publish = true) => {
    if (publish) eventListeners.forEach(listener=>listener(event));
    switch (event.type) {
      case 'device_cards': {
        const cards=acceptSnapshot(state.cards,event.data);
        revisions.set('cards',(revisions.get('cards')??0)+1);
        return set({cards,pending:reconcileScreens(state.pending,cards)});
      }
      case 'blocked_devices': return adopt('blocked', acceptSnapshot(state.blocked, event.data.snapshot));
      case 'device_control_completed': {
        const result=event.data.result;
        const failed=result.outcome==='failed'||result.outcome==='refused';
        const pending = state.pending.flatMap(p=>p.requestId!==event.data.requestId?[p]:!failed&&p.resource==='screen'?[{...p,confirmed:true}]:[]);
        set({ pending:reconcileScreens(pending,state.cards), error: result.outcome==='failed'||result.outcome==='refused' ? result.reason : null });
        return;
      }
      case 'update_status_changed': return adopt('update',event.data);
      case 'dnd_export_failed': return event.data === 'cancelled' ? undefined : set({error:'dnd_export_failed'});
      case 'dnd_preparation_failed': return set({error:'dnd_export_failed'});
      // Native shell owns activation intent; replaying this as shell.open would
      // turn a backend notification into a second user-activation request.
      case 'show_window': return;
      case 'wallpapers': return void query('wallpapers', async()=>Object.fromEntries((await api.wallpapers(false)).map(w=>[`${w.key.device}:${w.key.display}`,w.image]))).catch(()=>set({error:'unavailable'}));
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
      case 'file_provider_status': return adopt('fileProvider', event.data);
      case 'emergency_stop': return set({ pending: [], error: 'emergency_stop' });

    }
  };
  const query = async <K extends keyof AgentState>(key: K, fetch: () => Promise<AgentState[K]>) => {
    const sent = revisions.get(key) ?? 0, session = generation;
    const value = await fetch();
    if (generation !== session) return;
    if(key==='cards'&&value){receive({type:'device_cards',data:value as DeviceCardsView},false);return;}
    if(key==='blocked'&&value){adopt('blocked',acceptSnapshot(state.blocked,value as BlockedDevicesSnapshot));return;}
    if (key==='pairings' && !state.pairings) {
      adopt(key,value); const events=initialPairEvents; initialPairEvents=[]; events.forEach(event=>receive(event,false)); return;
    }
    if (key==='nearby' && !state.nearby) {
      adopt(key,value); const events=initialNearbyEvents; initialNearbyEvents=[]; events.forEach(event=>receive(event,false)); return;
    }
    if ((revisions.get(key) ?? 0) === sent) adopt(key, value);
  };
  return {
    get: () => state,
    subscribe: (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener); }; },
    receive, query, adopt,
    controlReply: (reply: DeviceControlReply) => { if (reply.type === 'device_control_completed') receive(reply); },
    onEvent: (listener:(event:ProductEvent)=>void) => {eventListeners.add(listener);return()=>{eventListeners.delete(listener);};},
    consumePairingEvent: (id:string) => set({pairingEvents:state.pairingEvents.filter(event=>event.data.event_id!==id)}),
    reset: () => { generation++; revisions.clear(); initialPairEvents=[]; initialNearbyEvents=[]; set(initial()); },
    fail: (reason: string) => set({ error: reason }),
    pending: (control: PendingControl) => {
      if (state.pending.some(p => p.device === control.device && p.resource === control.resource)) return false;
      set({ pending: [...state.pending, control] }); return true;
    },
    reject: (requestId: string, reason: string) => set({ pending: state.pending.filter(p => p.requestId !== requestId), error: reason }),
  };
}
export const agent = createAgentStore();
let stopped: (() => void) | null = null;
let starting: Promise<void> | null = null;
export function start(view = 'main'): Promise<void> {
  if (starting) return starting;
  starting = (async () => {
    try {
      stopped = await onAgentEvent(agent.receive);
      const boot = await shell.bootInfo();
      agent.adopt('boot', boot);
      if(view!=='main'&&view!=='panel'){await agent.query('localPreferences',()=>call<import('../settings/model').LocalPreferences>('ui_preferences'));return;}
      const settle = (queries: Promise<void>[]) => Promise.allSettled(queries.map(p => p.catch(error => agent.fail(error instanceof ApiFailure ? error.problem.code : 'unavailable'))));
      // These snapshots do not decide the initial route or its layout. In
      // particular, wallpaper extraction must not hold onboarding at alpha zero.
      void settle([
        agent.query('diagnostics', api.diagnosticsStatus), agent.query('update', api.updateStatus), agent.query('blocked', api.blockedDevices),
        agent.query('network', api.networkInfo), agent.query('nearby', pairingApi.nearby), agent.query('pairings', pairingApi.pairings),
        agent.query('wallpapers', async () => Object.fromEntries((await api.wallpapers(true)).map(w => [`${w.key.device}:${w.key.display}`, w.image]))),
      ]);
      await settle([
        agent.query('launch', api.launchAtLoginStatus), agent.query('localPreferences', () => call<import('../settings/model').LocalPreferences>('ui_preferences')), agent.query('report',()=>call<import('../settings/model').ReportStatus>('report_status')), agent.query('cards', api.deviceCards),
        agent.query('overview', api.overview), agent.query('layout', api.layout),
        agent.query('input', api.inputSettings),
        agent.query('clipboard', api.clipboardSettings), agent.query('audio', api.audioStatus),
        agent.query('permissions', api.permissions), agent.query('fileProvider', api.fileProviderStatus),
      ]);

    } catch { agent.fail('unavailable'); }
  })();
  return starting;
}
export function stop() { stopped?.(); stopped = null; starting = null; agent.reset(); }
export function useAgent(): AgentState { return useSyncExternalStore(agent.subscribe, agent.get); }
export function adoptLayout(layout: T.Layout) { agent.adopt('layout', layout); }
