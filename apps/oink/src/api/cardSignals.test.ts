import { describe, expect, it, vi } from 'vitest';
import { createAgentStore } from './store';
import type { DeviceCard, DeviceCardsView, DeviceCardSignals } from './cards';
const card = (device: string): DeviceCard => ({device,name:device,os:'windows',logo:'windows10',isSelf:false,section:'online',lastSeenMs:null,
  connection:{kind:'online',signal:{kind:'measured',rttMs:3,bars:3,colour:'green'}},pause:null,status:null});
export const cards = (revision='1', stream='s'):DeviceCardsView => ({schema:1,stream,revision,group:null,generatedAtMs:0,exitVolume:null,emergencyDelivery:null,cards:[card('a'),card('b')]});
const update=(rttMs:number|null, sampledAtMs=1000):DeviceCardSignals=>({stream:'s',revision:'1',sampledAtMs,signals:[{device:'a',rttMs},{device:'b',rttMs:3}]});
describe('card measurements',()=>{
  it('notifies only the changed device and preserves the global snapshot and event receipts',()=>{
    const store=createAgentStore();store.receive({type:'device_cards',data:cards()});
    const state=store.get(), all=vi.fn(), a=vi.fn(), b=vi.fn(), receipts=vi.fn();
    store.subscribe(all);store.onEvent(receipts);store.cardSignals.subscribe('a',a);store.cardSignals.subscribe('b',b);
    store.receive({type:'device_card_signals',data:update(23)});
    expect(store.get()).toBe(state);expect(all).not.toHaveBeenCalled();expect(receipts).not.toHaveBeenCalled();
    expect(a).toHaveBeenCalledTimes(1);expect(b).not.toHaveBeenCalled();expect(store.cardSignals.get('a')).toBe(23);
    store.receive({type:'device_card_signals',data:update(23,2000)});expect(a).toHaveBeenCalledTimes(1);
    store.receive({type:'device_control_completed',data:{requestId:'receipt',result:{outcome:'failed',reason:'apply_failed',unknownCode:null}}});
    expect(receipts).toHaveBeenCalledTimes(1);
  });
  it('rejects delayed samples across topology revisions and backend incarnations',()=>{
    const store=createAgentStore();store.receive({type:'device_cards',data:cards()});
    store.receive({type:'device_card_signals',data:update(23,2000)});
    store.receive({type:'device_card_signals',data:update(90,1000)});expect(store.cardSignals.get('a')).toBe(23);
    store.receive({type:'device_cards',data:cards('2')});
    store.receive({type:'device_card_signals',data:update(90,3000)});expect(store.cardSignals.get('a')).toBe(3);
    store.reset();store.receive({type:'device_cards',data:cards('1','new')});
    store.receive({type:'device_card_signals',data:update(90,4000)});expect(store.cardSignals.get('a')).toBe(3);
  });
  it('preserves newer measurements when an identical card query finishes and clears missing RTT',()=>{
    const store=createAgentStore();store.receive({type:'device_cards',data:cards()});
    store.receive({type:'device_card_signals',data:update(25)});
    store.receive({type:'device_cards',data:cards()});expect(store.cardSignals.get('a')).toBe(25);
    store.receive({type:'device_card_signals',data:update(null,2000)});expect(store.cardSignals.get('a')).toBeNull();
  });
});
