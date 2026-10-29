import {ApiFailure} from '../api/transport';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import type { DeviceControlReply, DeviceControlResult } from '../api/cards';
import { ControlActions, CONTROL_TIMEOUT_MS, SEND_INTERVAL_MS, controlResource, type ControlAction } from './controlActions';

beforeEach(()=>vi.useFakeTimers());
afterEach(()=>vi.useRealTimers());
const mouse=(desired:string):ControlAction=>({kind:'card',resource:'mouse',desired});
const volume=(value:number,extra:Partial<Extract<ControlAction,{kind:'speakerVolume'}>>={}):ControlAction=>({kind:'speakerVolume',output:false,volume:value,muted:false,...extra});
const value=(action:ControlAction)=>action.kind==='card'?action.desired:'mode' in action?action.mode:[action.volume,action.muted];
function owner() {
  const facts=new Map<string,{token:string;action:ControlAction}>();
  const writes:{id:string;device:string;action:ControlAction;token:string|null}[]=[];
  const fail=vi.fn(),show=vi.fn();
  let reply:(id:string,action:ControlAction)=>Promise<DeviceControlReply>=async requestId=>({type:'device_control_accepted',data:{requestId}});
  const key=(device:string,action:ControlAction)=>`${device}:${controlResource(action)}`;
  const controller=new ControlActions({
    read:(device,action)=>({token:facts.get(key(device,action))?.token??'v1',matches:JSON.stringify(value(facts.get(key(device,action))?.action??mouse('shared')))===JSON.stringify(value(action))}),
    send:(id,device,action)=>{writes.push({id,device,action,token:facts.get(key(device,action))?.token??'v1'});return reply(id,action);},
    refresh:async()=>{},show,fail,
  });
  const report=(action:ControlAction,token='v2',device='a')=>{facts.set(key(device,action),{token,action});controller.snapshot();};
  const complete=(index:number,result:DeviceControlResult={outcome:'applied'})=>controller.complete(writes[index].id,result);
  return {controller,writes,fail,show,report,complete,setReply:(next:typeof reply)=>{reply=next;}};
}
it.each<ControlAction>([
  mouse('local'),{kind:'card',resource:'keyboard',desired:'disabled'},
  {kind:'card',resource:'clipboard',desired:'local'},{kind:'card',resource:'game',desired:'on'},
  {kind:'card',resource:'screen',desired:'locked'},{kind:'card',resource:'pause',desired:true},
  {kind:'speaker',mode:'exit'},{kind:'microphone',mode:'source'},volume(90),
  {kind:'microphoneVolume',volume:110,muted:true},
])('shows and sends $kind immediately, with no acknowledgement dependency',action=>{
  const o=owner();o.controller.set('a',action);
  expect(o.controller.targets().get('a')).toEqual([action]);
  expect(o.writes[0].action).toEqual(action);
  o.controller.dispose();
});
it('never blocks unrelated resources or devices',()=>{
  const o=owner();
  o.controller.set('a',mouse('local'));
  o.controller.set('a',{kind:'card',resource:'keyboard',desired:'local'});
  o.controller.set('a',{kind:'speaker',mode:'exit'});
  o.controller.set('a',{kind:'microphone',mode:'source'});
  o.controller.set('b',mouse('local'));
  expect(o.writes).toHaveLength(5);
  o.controller.dispose();
});
it.each([true,false])('keeps the latest intent over earlier cards and receipts (cards first=%s)',cardsFirst=>{
  const o=owner();o.controller.set('a',mouse('local'));o.controller.set('a',mouse('disabled'));o.controller.set('a',mouse('shared'));
  expect(o.controller.targets().get('a')).toEqual([mouse('shared')]);
  expect(o.writes).toHaveLength(1);
  if(cardsFirst){o.report(mouse('local'));o.complete(0);}else{o.complete(0);o.report(mouse('local'));}
  expect(o.writes.map(w=>w.action)).toEqual([mouse('local'),mouse('shared')]);
  // A late duplicate refusal cannot roll back a newer action or report a toast.
  o.complete(0,{outcome:'failed',reason:'link_lost',unknownCode:null});
  expect(o.controller.targets().get('a')).toEqual([mouse('shared')]);
  expect(o.fail).not.toHaveBeenCalled();
  o.complete(1);o.report(mouse('shared'),'v3');
  expect(o.controller.targets().size).toBe(0);
  o.complete(0);
  expect(o.controller.targets().size).toBe(0);
});
it('out-of-order replies across resources retire only their own intents',()=>{
  const o=owner();const keyboard:ControlAction={kind:'card',resource:'keyboard',desired:'disabled'};
  o.controller.set('a',mouse('local'));o.controller.set('a',keyboard);
  o.report(keyboard);o.complete(1);
  expect(o.controller.targets().get('a')).toEqual([mouse('local')]);
  o.report(mouse('local'));o.complete(0);
  expect(o.controller.targets().size).toBe(0);
});
it.each(['failed','refused'] as const)('rolls back a %s result immediately and reports the existing reason',outcome=>{
  const o=owner();o.controller.set('a',mouse('local'));o.report(mouse('disabled'));
  o.complete(0,{outcome,reason:'apply_failed',unknownCode:null});
  expect(o.controller.targets().size).toBe(0); // caller reveals latest confirmed disabled
  expect(o.fail).toHaveBeenCalledExactlyOnceWith('apply_failed',mouse('local'));
});
it('a stale failure cannot discard the newer queued intent',()=>{
  const o=owner();o.controller.set('a',mouse('local'));o.controller.set('a',mouse('disabled'));
  o.complete(0,{outcome:'failed',reason:'link_lost',unknownCode:null});
  expect(o.writes).toHaveLength(2);
  expect(o.controller.targets().get('a')).toEqual([mouse('disabled')]);
  expect(o.fail).not.toHaveBeenCalled();o.controller.dispose();
});
it.each(['request','receipt','report'] as const)('bounds a missing %s, rolls back, and ignores late replies',async missing=>{
  const o=owner();let resolve!:(reply:DeviceControlReply)=>void;
  if(missing==='request')o.setReply(()=>new Promise(r=>{resolve=r;}));
  o.controller.set('a',mouse('local'));
  if(missing==='report')o.complete(0);
  await vi.advanceTimersByTimeAsync(CONTROL_TIMEOUT_MS);
  expect(o.controller.targets().size).toBe(0);
  expect(o.fail).toHaveBeenCalledExactlyOnceWith('outcome_unknown',mouse('local'));
  if(resolve)resolve({type:'device_control_completed',data:{requestId:o.writes[0].id,result:{outcome:'applied'}}});
  o.complete(0,{outcome:'failed',reason:'link_lost',unknownCode:null});
  await Promise.resolve();
  expect(o.fail).toHaveBeenCalledTimes(1);
  o.controller.set('a',mouse('disabled'));expect(o.writes).toHaveLength(2);o.controller.dispose();
});
it('transport failure releases the intent without awaiting a refresh',async()=>{
  const fail=vi.fn();
  const c=new ControlActions({read:()=>({token:'v1',matches:false}),send:async()=>{throw Error('offline');},
    refresh:()=>new Promise(()=>{}),show:()=>{},fail});
  c.set('a',mouse('local'));await Promise.resolve();
  expect(c.targets().size).toBe(0);expect(fail).toHaveBeenCalledExactlyOnceWith('outcome_unknown',mouse('local'));
});
it('keeps an applied receipt through a failed refresh until live facts arrive',async()=>{
  let observed=false;
  const fail=vi.fn();
  const c=new ControlActions({read:()=>({token:observed?'v2':'v1',matches:observed}),
    send:async requestId=>({type:'device_control_completed',data:{requestId,result:{outcome:'applied'}}}),
    refresh:async()=>{throw Error('refresh disconnected');},show:()=>{},fail});
  c.set('a',mouse('local'));
  await vi.advanceTimersByTimeAsync(1);
  expect(c.targets().get('a')).toEqual([mouse('local')]);
  expect(fail).not.toHaveBeenCalled();
  observed=true;c.snapshot();
  expect(c.targets().size).toBe(0);
  await vi.advanceTimersByTimeAsync(CONTROL_TIMEOUT_MS);
  expect(fail).not.toHaveBeenCalled();c.dispose();
});
it('rebases stale revisions on fresh reports, never retries an obsolete intent',()=>{
  const o=owner();o.controller.set('a',mouse('local'));
  o.complete(0,{outcome:'refused',reason:'stale_revision',unknownCode:null});
  expect(o.writes).toHaveLength(1);
  o.controller.set('a',mouse('disabled'));o.report(mouse('shared'));
  expect(o.writes.map(w=>[w.token,w.action])).toEqual([['v1',mouse('local')],['v2',mouse('disabled')]]);
  expect(o.fail).not.toHaveBeenCalled();o.controller.dispose();
});
it('a permanently stale token times out rather than leaving an optimistic state stuck',async()=>{
  const o=owner();o.controller.set('a',mouse('local'));
  o.complete(0,{outcome:'refused',reason:'stale_revision',unknownCode:null});
  await vi.advanceTimersByTimeAsync(CONTROL_TIMEOUT_MS);
  expect(o.writes).toHaveLength(1);expect(o.controller.targets().size).toBe(0);
  expect(o.fail).toHaveBeenCalledExactlyOnceWith('outcome_unknown',mouse('local'));
});
it('disposal retires timers and ignores callbacks even if the same controller is reused',async()=>{
  const o=owner();o.controller.set('a',mouse('local'));o.controller.dispose();
  o.controller.set('a',mouse('disabled'));o.complete(0);
  expect(o.controller.targets().get('a')).toEqual([mouse('disabled')]);
  o.controller.dispose();await vi.advanceTimersByTimeAsync(CONTROL_TIMEOUT_MS);
  expect(o.fail).not.toHaveBeenCalled();
});
it('bounds a fast volume drag and sends the durable release immediately',async()=>{
  const o=owner();let revision=1;
  o.setReply(async(id,action)=>{
    queueMicrotask(()=>o.report(action,`v${++revision}`));
    return {type:'device_control_completed',data:{requestId:id,result:{outcome:'applied'}}};
  });
  for(let i=0;i<100;i++){o.controller.set('a',volume(51+i%50,{deferPersistence:true}));await vi.advanceTimersByTimeAsync(8);}
  o.controller.set('a',volume(137,{final:true}));await vi.advanceTimersByTimeAsync(0);
  expect(o.writes.at(-1)!.action).toEqual(volume(137,{final:true}));
  expect(o.writes.length).toBeLessThanOrEqual(Math.ceil(800/SEND_INTERVAL_MS)+2);
  expect(o.writes.length).toBeGreaterThan(5);
  expect(new Set(o.writes.map(w=>w.token)).size).toBe(o.writes.length);
  expect(o.controller.targets().size).toBe(0);
});
it('flushes the identical release value after a deferred drag already reached the cards',async()=>{
  const o=owner();o.controller.set('a',volume(60,{deferPersistence:true}));o.report(volume(60));o.complete(0);
  expect(o.controller.targets().size).toBe(0);
  o.controller.set('a',volume(60,{final:true}));
  expect(o.writes).toHaveLength(2);expect(o.writes[1].action).toEqual(volume(60,{final:true}));o.controller.dispose();
});
it('orders mode and volume writes together while showing the latest of each',()=>{
  const o=owner();const exit:ControlAction={kind:'speaker',mode:'exit'};
  o.controller.set('a',exit);o.controller.set('a',volume(90));o.controller.set('a',volume(110,{final:true}));
  expect(o.controller.targets().get('a')).toEqual([exit,volume(110,{final:true})]);
  expect(o.writes).toHaveLength(1);
  o.report(exit);o.complete(0);
  expect(o.writes[1].action).toEqual(volume(110,{final:true}));o.controller.dispose();
});
it('sends a superseding intent after an uncertain failure even when the last report already matches',()=>{
  const o=owner();o.controller.set('a',mouse('local'));o.controller.set('a',mouse('shared'));
  o.complete(0,{outcome:'failed',reason:'outcome_unknown',unknownCode:null});
  expect(o.writes.map(w=>w.action)).toEqual([mouse('local'),mouse('shared')]);
  expect(o.controller.targets().get('a')).toEqual([mouse('shared')]);
  o.controller.dispose();
});

it.each(['microphone','microphoneVolume'] as const)('preserves typed %s failures for its toast without replacing them with unknown',async kind=>{
  const action:ControlAction=kind==='microphone'?{kind,mode:'receiver'}:{kind,volume:80,muted:false};
  const fail=vi.fn();
  const c=new ControlActions({read:()=>({token:'v1',matches:false}),send:async()=>{throw new ApiFailure({code:'internal',problem:{code:'permission_denied',unknownCode:null,nativeCode:null,retryAfterMs:null}});},refresh:async()=>{},show:()=>{},fail});
  c.set('a',action);await Promise.resolve();
  expect(fail).toHaveBeenCalledExactlyOnceWith('permission_denied',action);
  expect(c.targets().size).toBe(0);c.dispose();
});

it('coalesces waiting volumes, refreshes once per intent, and bounds a permanently missing owner',async()=>{
  const send=vi.fn(),refresh=vi.fn(async()=>{}),fail=vi.fn();
  const c=new ControlActions({read:()=>({token:null,matches:false,volumeReadiness:'wait'}),send,refresh,show:()=>{},fail});
  c.set('a',volume(60));await Promise.resolve();
  c.snapshot();c.snapshot();expect(refresh).toHaveBeenCalledTimes(1);
  c.set('a',volume(70,{final:true}));await Promise.resolve();
  expect(c.targets().get('a')).toEqual([volume(70,{final:true})]);
  await vi.advanceTimersByTimeAsync(CONTROL_TIMEOUT_MS);
  expect(send).not.toHaveBeenCalled();expect(c.targets().size).toBe(0);
  expect(fail).toHaveBeenCalledExactlyOnceWith('outcome_unknown',volume(70,{final:true}));
});
it('a later mode intent cancels an obsolete waiting volume without blocking the lane',()=>{
  const send=vi.fn(async(requestId:string)=>({type:'device_control_accepted' as const,data:{requestId}})),fail=vi.fn();
  const c=new ControlActions({read:(_d,a)=>({token:null,matches:false,volumeReadiness:a.kind==='speakerVolume'?'wait':undefined}),send,refresh:async()=>{},show:()=>{},fail});
  c.set('a',volume(60,{output:true}));c.set('a',{kind:'speaker',mode:'unused'});
  expect(send).toHaveBeenCalledTimes(1);expect(send.mock.calls[0]).toEqual([expect.any(String),'a',{kind:'speaker',mode:'unused'}]);
  expect(fail).not.toHaveBeenCalled();c.dispose();
});
