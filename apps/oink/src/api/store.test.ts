import { describe, expect, it, vi } from 'vitest';
import { shell } from './client';
import { acceptSnapshot, createAgentStore } from './store';
import type { DeviceCardsView } from './cards';
import type { PairingSnapshot, PairView, NearbySnapshot } from './pairing';
const view = (revision: string, stream = 'current'): DeviceCardsView => ({ schema: 1, stream, revision, group: null, generatedAtMs: 0, exitVolume: null, emergencyDelivery: null, cards: [] });
describe('authenticated presentation ordering', () => {
  it('keeps an event that arrives before an initial query reply', async () => {
    const store = createAgentStore();
    let complete!: (v: DeviceCardsView) => void;
    const query = store.query('cards', () => new Promise(r => { complete = r; }));
    store.receive({ type: 'device_cards', data: view('9007199254740994') });
    complete(view('9007199254740993'));
    await query;
    expect(store.get().cards?.revision).toBe('9007199254740994');
  });
  it('rejects repeated, reversed and retired-stream snapshots', () => {
    const current = view('12');
    expect(acceptSnapshot(current, view('12'))).toBe(current);
    expect(acceptSnapshot(current, view('11'))).toBe(current);
    expect(acceptSnapshot(current, view('13', 'retired'))).toBe(current);
    expect(acceptSnapshot(current, view('13'))?.revision).toBe('13');
  });
  it('retires an outstanding query and its pending optimistic state on reconnect', async () => {
    const store = createAgentStore();
    let complete!: (v: DeviceCardsView) => void;
    const query = store.query('cards', () => new Promise(r => { complete = r; }));
    store.pending({ requestId: 'request', device: 'device', resource: 'mouse', desired: 'local' });
    store.reset(); complete(view('999', 'retired')); await query;
    expect(store.get().cards).toBeNull(); expect(store.get().pending).toEqual([]);
  });
  it('does not equate accepted, refused or unknown with applied owner facts', () => {
    const store = createAgentStore();
    store.receive({ type: 'device_cards', data: view('1') });
    expect(store.pending({requestId:'one',device:'device',resource:'mouse',desired:'local'})).toBe(true);
    expect(store.pending({requestId:'two',device:'device',resource:'mouse',desired:'shared'})).toBe(false);
    expect(store.get().cards?.revision).toBe('1');
    store.receive({type:'device_control_completed',data:{requestId:'one',result:{outcome:'failed',reason:'outcome_unknown',unknownCode:null}}});
    expect(store.get().pending).toEqual([]);
    expect(store.get().cards?.revision).toBe('1');
    expect(store.get().error).toBe('outcome_unknown');
  });
  it('protects the blocked-device snapshot against a late query', async () => {
    const store = createAgentStore();
    let complete!: (value: {stream:string;revision:string;entries:[]}) => void;
    const query = store.query('blocked', () => new Promise(resolve => { complete = resolve; }));
    store.receive({type:'blocked_devices',data:{snapshot:{stream:'blocks',revision:'2',entries:[{device:'blocked',name:null,os:null,blocked_at_ms:1}]}}});
    complete({stream:'blocks',revision:'1',entries:[]}); await query;
    expect(store.get().blocked?.entries).toHaveLength(1);
  });
  it('applies an early session delta after the initial snapshot without dropping pending requests',async()=>{
    const store=createAgentStore();let complete!:(value:PairingSnapshot)=>void;
    const query=store.query('pairings',()=>new Promise(resolve=>{complete=resolve;}));
    const session:PairView={id:'session',revision:'2',direction:'incoming',peer:{address:{family:0,address:[192,0,2,1],port:47470,interface_scope:0},expected_id:null,verified:null},phase:2,attempts_left:3,feedback:'waiting',terminal:null};
    store.receive({type:'pairing_changed',data:{revision:'2',view:session}});
    complete({revision:'1',requests:[{id:'pending',revision:'1',peer:{device:'2'.repeat(64),name:'Device',os:'windows',remote:session.peer.address,listen:null},source:{group:'source',root:'source'},target:{group:'target',root:'target'},source_count:2,target_count:1}],sessions:[]});await query;
    expect(store.get().pairings?.requests).toHaveLength(1);expect(store.get().pairings?.sessions[0].id).toBe('session');
  });
  it('buffers nearby removal until the snapshot and rejects a lower snapshot revision',async()=>{
    const store=createAgentStore();let complete!:(value:NearbySnapshot)=>void;
    const query=store.query('nearby',()=>new Promise(resolve=>{complete=resolve;}));
    store.receive({type:'nearby_changed',data:{revision:'3',upserts:[],removed:['removed']}});
    complete({revision:'2',rows:[]});await query;
    store.receive({type:'nearby_snapshot',data:{revision:'1',rows:[]}});
    expect(store.get().nearby?.revision).toBe('3');
  });
  it('keeps applied preference events newer than a pending command reply',async()=>{
    const store=createAgentStore();
    const before:import('./cards').UiPreferences={show_status_icon:true,confirm_quit:true,ui_language:'auto',theme_mode:'light',theme_accent:{light:'#2E7BFF',dark:'#2E7BFF'},log_level:'info',log_max_mb:1024};
    let complete!:(value:typeof before)=>void;
    const command=store.query('localPreferences',()=>new Promise(resolve=>{complete=resolve;}));
    store.receive({type:'ui_preferences',data:{...before,theme_mode:'dark'}});
    complete(before);await command;
    expect(store.get().localPreferences?.theme_mode).toBe('dark');
  });
  it('never displays an older consent grant over a newer revocation',()=>{
    const store=createAgentStore(), status={configured:true,delivery_disabled:false,consent:{choice:'denied' as const,policy_version:3,generation:2}};
    store.receive({type:'report_status_changed',data:status});
    store.receive({type:'report_status_changed',data:{...status,consent:{...status.consent,choice:'granted',generation:1}}});
    expect(store.get().report?.consent.choice).toBe('denied');
  });
});

describe('native window activation ownership', () => {
  it('does not replay backend shows as a user window command', () => {
    const open = vi.spyOn(shell, 'open').mockResolvedValue(undefined);
    createAgentStore().receive({type:'show_window'});
    expect(open).not.toHaveBeenCalled();
    open.mockRestore();
  });
});
