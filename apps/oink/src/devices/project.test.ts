// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';
import type { DeviceCard, DeviceCardsView, ResourceCard, SharingMode } from '../api/cards';
import { createAgentStore } from '../api/store';
import { presentCards } from './project';
import { screenValue } from './data';
import { setLanguage, problemText } from '../i18n';
import { indicators } from './DeviceList';

const unavailable = {kind:'unavailable',reason:'state_unknown'} as const;
const resource = (): ResourceCard<SharingMode> => ({configuredMode:'shared',icon:'shared',fault:{code:'none',unknownCode:null,problem:null},condition:'ready',action:unavailable});
const card = (): DeviceCard => ({
  device:'2'.repeat(64),name:'Remote device',os:'windows',logo:'windows10',isSelf:false,section:'online',lastSeenMs:null,
  connection:{kind:'online',signal:{kind:'unknown',rttMs:null,bars:0,colour:'grey'}},pause:null,status:{
    actions:{screenShared:unavailable,screenLocal:unavailable,mouseShared:unavailable,mouseLocal:unavailable,keyboardShared:unavailable,keyboardLocal:unavailable,keyboardDisabled:unavailable,keyboardEnabled:unavailable,clipboardShared:unavailable,clipboardLocal:unavailable,forwardingEnabled:unavailable,forwardingDisabled:unavailable},
    cursor:{icon:'unknown',focusDevice:null,focusName:null,lockedDevice:null,context:'unknown'},
    screen:resource(),mouse:resource(),keyboard:{...resource(),disabled:false},clipboard:resource(),
    speaker:{...resource(),configuredMode:null,icon:'unknown',makeExitAction:unavailable,forwardingControlsToken:null,forwardingEnabled:null,activity:'unknown',receiverDevice:null,receiverName:null,activeSources:null,receiveReady:null,forwarding:null},
    game:{preference:null,active:null,detected:null,actions:{auto:unavailable,on:unavailable,off:unavailable}},
  },
});
const view = (device:DeviceCard):DeviceCardsView => ({schema:1,stream:'test',revision:'1',group:null,generatedAtMs:0,exitVolume:null,emergencyDelivery:null,cards:[device]});
describe('owner card presentation',()=>{
  it.each(['shared','disabled'] as const)('reconciles Locked to %s after owner completion and focus facts in either order',desired=>{
    for (const completionFirst of [true,false]) {
      const store=createAgentStore(), original=card();
      original.status!.cursor={icon:'locked',focusDevice:original.device,focusName:original.name,lockedDevice:original.device,context:'group'};
      store.receive({type:'device_cards',data:view(original)});
      store.pending({requestId:'screen',device:original.device,resource:'screen',desired});
      const shown=()=>screenValue(presentCards(store.get().cards,store.get().pending)[0]);
      expect(shown()).toBe(desired);
      const completion=()=>store.controlReply({type:'device_control_completed',data:{requestId:'screen',result:{outcome:desired==='shared'?'no_change':'applied'}}});
      const facts=()=>{
        const unlocked=card();
        unlocked.status!.cursor={...original.status!.cursor,icon:'local',lockedDevice:null};
        unlocked.status!.screen.configuredMode=desired==='shared'?'shared':'local';
        store.receive({type:'device_cards',data:{...view(unlocked),revision:'2'}});
      };
      if(completionFirst){completion();expect(shown()).toBe(desired);facts();}
      else {facts();expect(shown()).toBe(desired);completion();}
      expect(store.get().pending).toEqual([]);
      expect(shown()).toBe(desired);
      store.receive({type:'device_cards',data:view(original)});
      expect(shown()).toBe(desired);
    }
  });
  it('restores Locked and the typed reason when a screen command is refused',()=>{
    const store=createAgentStore(), original=card();
    original.status!.cursor.icon='locked';
    store.receive({type:'device_cards',data:view(original)});
    store.pending({requestId:'screen',device:original.device,resource:'screen',desired:'disabled'});
    expect(screenValue(presentCards(store.get().cards,store.get().pending)[0])).toBe('disabled');
    store.controlReply({type:'device_control_completed',data:{requestId:'screen',result:{outcome:'refused',reason:'stale_revision',unknownCode:null}}});
    expect(screenValue(presentCards(store.get().cards,store.get().pending)[0])).toBe('locked');
    expect(store.get().error).toBe('stale_revision');
  });
  it('preserves unavailable signal, cursor, role, volume and game observations',()=>{
    const [device] = presentCards(view(card()),[]);
    expect(device.latency).toBeNull(); expect(device.cursor).toBe('unknown');
    expect(device.speaker.mode).toBe('unknown'); expect(device.speaker.volumeKnown).toBe(false);
    expect(device.speaker.canSetVolume).toBe(false); expect(device.gameMode).toBeNull(); expect(device.gameActive).toBeNull();
  });
  it.each(['refused','failed'] as const)('rolls optimistic remote state back on %s completion',outcome=>{
    const store=createAgentStore(), original=card();
    store.receive({type:'device_cards',data:view(original)});
    store.pending({requestId:'request',device:original.device,resource:'mouse',desired:'local'});
    expect(presentCards(store.get().cards,store.get().pending)[0].mouse).toBe('local');
    store.receive({type:'device_control_completed',data:{requestId:'request',result:{outcome,reason:outcome==='failed'?'outcome_unknown':'stale_revision',unknownCode:null}}});
    expect(presentCards(store.get().cards,store.get().pending)[0].mouse).toBe('shared');
  });
  it('shows one prospective exit and restores the existing exit after refusal',()=>{
    const old=card(),next=card();next.device='3'.repeat(64);
    old.status!.speaker.icon='exit';
    const facts={...view(old),cards:[old,next]};
    const optimistic=presentCards(facts,[{requestId:'exit',device:next.device,resource:'speaker',desired:'exit'}]);
    expect(optimistic.filter(d=>d.speaker.mode==='exit').map(d=>d.id)).toEqual([next.device]);
    expect(presentCards(facts,[]).filter(d=>d.speaker.mode==='exit').map(d=>d.id)).toEqual([old.device]);
  });
  it('shows a missing-exit warning only from the owner observation',()=>{
    const source=card();source.status!.speaker.icon='forwarding';source.status!.speaker.activity='unknown';
    expect(indicators(presentCards(view(source),[])[0],true).find(i=>i.glyph==='speaker')?.badge).not.toBe('warning');
    source.status!.speaker.activity='waiting_for_exit';
    expect(indicators(presentCards(view(source),[])[0],true).find(i=>i.glyph==='speaker')?.badge).toBe('warning');
  });
});

describe('unavailable resource presentation',()=>{
  it.each(['en','zh-Hans'] as const)('uses warning badges and prototype copy in %s',language=>{
    setLanguage(language);
    const missing=card();missing.status=null;
    const state=indicators(presentCards(view(missing),[])[0]);
    expect(state).toHaveLength(4);
    for(const indicator of state) {
      expect(indicator.tone).toBe('warn');
      expect(indicator.badge).toBe('warning');
      expect(indicator.label).toContain(language==='en'?'unavailable':'不可用');
      expect(indicator.label).not.toMatch(/state|状态/);
    }
    expect(problemText('unrecognized')).toBe(language==='en'?'Unavailable':'不可用');
  });
});


describe('live audio resource observations',()=>{
  it.each(['en','zh-Hans'] as const)('updates owner audio icons after denial and recovery in %s',language=>{
    setLanguage(language);
    const store=createAgentStore();
    for(const [index,denied] of [false,true,false].entries()) {
      const observed=card();
      observed.status!.speaker={...observed.status!.speaker,configuredMode:'source',forwardingEnabled:true,
        icon:denied?'fault':'forwarding',condition:denied?'faulted':'ready',activity:denied?'failed':'forwarding',receiverDevice:'3'.repeat(64),
        fault:{code:denied?'permission_denied':'none',unknownCode:null,problem:null}};
      store.receive({type:'device_cards',data:{...view(observed),revision:String(index+1)}});
      const device=presentCards(store.get().cards,[])[0];
      const speaker=indicators(device).find(i=>i.glyph==='speaker')!;
      expect(device.speaker.mode).toBe(denied?'fault':'forwarding');
      expect(speaker.tone).toBe(denied?'warn':'good');
      expect(speaker.badge==='warning').toBe(denied);
      if(!denied)expect(speaker.label).not.toContain(language==='en'?'unavailable':'不可用');
    }
  });
});

it.each(['en','zh-Hans'] as const)('shows a paired source with forwarding disabled as local in %s', language => {
  setLanguage(language);
  const local = card();
  local.status!.speaker = {...local.status!.speaker, configuredMode:'unused', icon:'unused', forwardingEnabled:false, activity:'idle'};
  const device = presentCards(view(local), [])[0];
  expect(device.speaker.mode).toBe('unused');
  const indicator = indicators(device, true).find(i => i.glyph === 'speaker')!;
  expect(indicator.label).toBe(language === 'en' ? 'Audio output: local' : '音频输出：本地');
  expect(indicator.badge).toBeUndefined();
});

it.each([
  ['en','selected output unavailable','no group output'],
  ['zh-Hans','所选出口不可用','无出口'],
  ['ja','選択した出力先を利用できません','出力先なし'],
  ['ko','선택한 출력 기기를 사용할 수 없음','출력 기기 없음'],
] as const)('distinguishes an unavailable selected exit from no exit in %s', (language, unavailableText, absentText) => {
  setLanguage(language);
  const source=card();
  source.status!.speaker={...source.status!.speaker,configuredMode:'source',icon:'forwarding',forwardingEnabled:true,
    activity:'waiting_for_exit',receiverDevice:'3'.repeat(64)};
  const selected=indicators(presentCards(view(source),[])[0]).find(i=>i.glyph==='speaker')!;
  expect(selected.badge).toBe('warning');
  expect(selected.tip).toContain(unavailableText);
  source.status!.speaker={...source.status!.speaker,activity:'idle',receiverDevice:null};
  const absent=indicators(presentCards(view(source),[])[0]).find(i=>i.glyph==='speaker')!;
  expect(absent.badge).toBe('warning');
  expect(absent.tip).toContain(absentText);
  expect(absent.tip).not.toContain(unavailableText);
});
