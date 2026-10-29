// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';
import type { DeviceCard, DeviceCardsView, ResourceCard, SharingMode } from '../api/cards';
import { createAgentStore } from '../api/store';
import { presentCards, presentControls, resourceAction } from './project';
import { ControlActions } from './controlActions';
import { controlObservation } from './controlPlan';
import { screenValue } from './data';
import { setLanguage, problemText } from '../i18n';
import { indicators } from './DeviceList';
import { captureNotices } from '../api/notices.fixture';
const notices = captureNotices();

const unavailable = {kind:'unavailable',reason:'state_unknown'} as const;
const resource = (): ResourceCard<SharingMode> => ({configuredMode:'shared',icon:'shared',fault:{code:'none',unknownCode:null,problem:null},condition:'ready',action:unavailable});
const card = (): DeviceCard => ({
  device:'2'.repeat(64),name:'Remote device',os:'windows',logo:'windows10',isSelf:false,section:'online',lastSeenMs:null,
  connection:{kind:'online',signal:{kind:'unknown',rttMs:null,bars:0,colour:'grey'}},pause:null,status:{
    actions:{screenShared:unavailable,screenLocal:unavailable,mouseShared:unavailable,mouseLocal:unavailable,keyboardShared:unavailable,keyboardLocal:unavailable,keyboardDisabled:unavailable,keyboardEnabled:unavailable,clipboardShared:unavailable,clipboardLocal:unavailable,forwardingEnabled:unavailable,forwardingDisabled:unavailable},
    cursor:{icon:'unknown',focusDevice:null,focusName:null,lockedDevice:null,context:'unknown'},
    screen:resource(),mouse:resource(),keyboard:{...resource(),disabled:false},clipboard:resource(),
    speaker:{...resource(),configuredMode:null,icon:'unknown',makeExitAction:unavailable,forwardingControlsToken:null,localOutput:null,forwardingEnabled:null,activity:'unknown',receiverDevice:null,receiverName:null,activeSources:null,receiveReady:null,forwarding:null},
    game:{preference:null,active:null,detected:null,actions:{auto:unavailable,on:unavailable,off:unavailable}},
  },
});
const view = (device:DeviceCard):DeviceCardsView => ({schema:1,stream:'test',revision:'1',group:null,generatedAtMs:0,exitVolume:null,emergencyDelivery:null,cards:[device]});
function controlStore() {
  const store=createAgentStore();
  let id='';
  const controls=new ControlActions({read:(device,action)=>controlObservation(store.get().cards,device,action),
    send:async requestId=>{id=requestId;return {type:'device_control_accepted',data:{requestId}};},
    show:()=>{},refresh:async()=>{},fail:store.fail});
  store.onEvent(event=>event.type==='device_control_completed'&&controls.complete(event.data.requestId,event.data.result));
  return {...store,controls,
    intent:(device:string,resource:'screen'|'mouse',desired:string)=>controls.set(device,{kind:'card',resource,desired}),
    complete:(result:import('../api/cards').DeviceControlResult)=>store.controlReply({type:'device_control_completed',data:{requestId:id,result}}),
    shown:()=>presentControls(store.get().cards,controls.targets()),
    receive:(event:import('../api/store').ProductEvent)=>{store.receive(event);controls.snapshot();},
  };
}
describe('owner card presentation',()=>{
  it('uses only explicit remote AX reports, leaving legacy faults and Windows alone',()=>{
    const owner=card();owner.os='macos';
    owner.status!.mouse.fault={code:'permission_denied',unknownCode:null,problem:null};
    owner.status!.keyboard.fault={code:'permission_denied',unknownCode:null,problem:null};
    expect(presentCards(view(owner),[])[0].accessibilityMissing).toBe(false);
    owner.status!.accessibilityGranted=false;
    expect(presentCards(view(owner),[])[0].accessibilityMissing).toBe(true);
    owner.status!.accessibilityGranted=true;
    expect(presentCards(view(owner),[])[0].accessibilityMissing).toBe(false);
    owner.os='windows';owner.status!.accessibilityGranted=false;
    expect(presentCards(view(owner),[])[0].accessibilityMissing).toBe(false);
  });
  it('uses the local permissions API even when the local card has a stale AX report',()=>{
    const owner=card();owner.os='macos';owner.isSelf=true;owner.status!.accessibilityGranted=false;
    expect(presentCards(view(owner),[],[])[0].accessibilityMissing).toBe(false);
    for(const state of ['NotGranted','NotDetermined','Denied','Granted','NotApplicable'] as const) {
      expect(presentCards(view(owner),[],[{kind:'accessibility',state,required:true}])[0].accessibilityMissing)
        .toBe(!['Granted','NotApplicable'].includes(state));
    }
  });

  it('uses local Input Monitoring permission before a capture fault without applying it to peers',()=>{
    const owner=card();owner.os='macos';owner.isSelf=true;
    const denied=[{kind:'input_monitoring' as const,state:'Denied' as const,required:true}];
    expect(presentCards(view(owner),[],denied)[0].inputPermissionMissing).toBe(true);
    owner.isSelf=false;
    expect(presentCards(view(owner),[],denied)[0].inputPermissionMissing).toBe(false);
    owner.status!.mouse.fault={code:'permission_denied',unknownCode:null,problem:null};
    expect(presentCards(view(owner),[],denied)[0].inputPermissionMissing).toBe(true);
  });

  it('offers mouse disable only when the owner supplies an invokable action',()=>{
    const device=card();
    expect(presentCards(view(device),[])[0].canDisableMouse).toBe(false);
    device.status!.actions.mouseDisabled=unavailable;
    expect(presentCards(view(device),[])[0].canDisableMouse).toBe(false);
    device.status!.actions.mouseDisabled={kind:'invoke',command:'set_local_option',token:'mouse-disable'};
    expect(presentCards(view(device),[])[0].canDisableMouse).toBe(true);
  });
  it.each(['shared','disabled'] as const)('reconciles Locked to %s after owner completion and focus facts in either order',desired=>{
    for (const completionFirst of [true,false]) {
      const store=controlStore(), original=card();
      original.status!.cursor={icon:'locked',focusDevice:original.device,focusName:original.name,lockedDevice:original.device,context:'group'};
      store.receive({type:'device_cards',data:view(original)});
      store.intent(original.device,'screen',desired);
      const shown=()=>screenValue(store.shown()[0]);
      expect(shown()).toBe(desired);
      const completion=()=>store.complete({outcome:desired==='shared'?'no_change':'applied'});
      const facts=()=>{
        const unlocked=card();
        unlocked.status!.cursor={...original.status!.cursor,icon:'local',lockedDevice:null};
        unlocked.status!.screen.configuredMode=desired==='shared'?'shared':'local';
        store.receive({type:'device_cards',data:{...view(unlocked),revision:'2'}});
      };
      if(completionFirst){completion();expect(shown()).toBe(desired);facts();}
      else {facts();expect(shown()).toBe(desired);completion();}
      expect(store.controls.targets().size).toBe(0);
      expect(shown()).toBe(desired);
      store.receive({type:'device_cards',data:view(original)});
      expect(shown()).toBe(desired);
    }
  });
  it('restores Locked and the typed reason when a screen command is refused',()=>{
    const store=controlStore(), original=card();
    original.status!.cursor.icon='locked';
    store.receive({type:'device_cards',data:view(original)});
    store.intent(original.device,'screen','disabled');
    expect(screenValue(store.shown()[0])).toBe('disabled');
    store.complete({outcome:'refused',reason:'apply_failed',unknownCode:null});
    expect(screenValue(store.shown()[0])).toBe('locked');
    expect(notices()).toContain('apply_failed');
  });
  it.each([true,false])('maps readable local output for self=%s and preserves null',isSelf=>{
    const owner=card();owner.isSelf=isSelf;
    expect(presentCards(view(owner),[])[0].speaker.localOutput).toBeNull();
    owner.status!.speaker.localOutput={volumePercent:73,muted:true};
    expect(presentCards(view(owner),[])[0].speaker.localOutput).toEqual({volume:73,muted:true});
  });
  it('preserves unavailable signal, cursor, role, volume and game observations',()=>{
    const [device] = presentCards(view(card()),[]);
    expect(device.latency).toBeNull(); expect(device.cursor).toBe('unknown');
    expect(device.speaker.mode).toBe('unknown'); expect(device.speaker.volumeKnown).toBe(false);
    expect(device.speaker.canSetVolume).toBe(false); expect(device.gameMode).toBeNull(); expect(device.gameActive).toBeNull();
  });
  it.each(['refused','failed'] as const)('rolls optimistic remote state back on %s completion',outcome=>{
    const store=controlStore(), original=card();
    store.receive({type:'device_cards',data:view(original)});
    store.intent(original.device,'mouse','local');
    expect(store.shown()[0].mouse).toBe('local');
    store.complete({outcome,reason:outcome==='failed'?'outcome_unknown':'apply_failed',unknownCode:null});
    expect(store.shown()[0].mouse).toBe('shared');
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
    expect(state[3]).toMatchObject({glyph:"microphone",tone:"muted",badge:"mute"});
    for(const indicator of state.slice(0,3)) {
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

it.each(['en','zh-Hans'] as const)('keeps an unresolved exit distinct from no exit in %s', language => {
  setLanguage(language);
  const source=card();
  source.status!.speaker={...source.status!.speaker,configuredMode:'source',icon:'forwarding',forwardingEnabled:true,
    activity:'unknown',receiverDevice:null};
  const device=presentCards(view(source),[])[0];
  const indicator=indicators(device,true).find(i=>i.glyph==='speaker')!;
  expect(device.speaker.mode).toBe('forwarding');
  expect(indicator.badge).toBe('warning');
  expect(indicator.tip).toContain(language==='en'?'Waiting for an audio exit':'等待音频出口');
});

it.each(['mouse','keyboard','microphone','speaker','mute','volume'])('projects %s without pending presentation', resource => {
  const owner=card();
  const projected=presentCards(view(owner),[{requestId:'pending',device:owner.device,resource,desired:resource==='volume'?80:resource==='mute'?true:'shared'}])[0];
  expect('pending' in projected).toBe(false);
  expect('pending' in projected.speaker).toBe(false);
});

it('releases a failed receiver using its claim action, not the forwarding preference',()=>{
  const c=card();
  c.status!.speaker={...c.status!.speaker,configuredMode:'receiver',icon:'fault',action:{kind:'invoke',token:'release',command:'clear_audio_exit'}};
  expect(resourceAction(c,'speaker','unused')).toBe(c.status!.speaker.action);
  expect(presentCards(view(c),[])[0].speaker.configuredMode).toBe('receiver');
});

it('updates configured roles in optimistic speaker controls, including a failed old exit',()=>{
  const old=card(), next=card();next.device='3'.repeat(64);
  old.status!.speaker={...old.status!.speaker,configuredMode:'receiver',icon:'fault'};
  const facts={...view(old),cards:[old,next]};
  const shown=presentControls(facts,new Map([[next.device,[{kind:'speaker',mode:'exit'}]]]));
  expect(shown.map(d=>d.speaker.configuredMode)).toEqual(['unused','receiver']);
  const stopped=presentControls(view(old),new Map([[old.device,[{kind:'speaker',mode:'unused'}]]]));
  expect(stopped[0].speaker.configuredMode).toBe('unused');
});


it.each(['shared','local'] as const)('keeps the frozen guard unlock command when selecting keyboard %s', desired => {
  const c=card();
  c.status!.keyboard.disabled=true;
  const action={kind:'invoke',token:'unlock',command:'set_local_option'} as const;
  c.status!.actions.keyboardEnabled=action;
  expect(resourceAction(c,'keyboard',desired)).toBe(action);
});

it('projects microphone faults and keeps internal capture/consumer activity out of card text', () => {
  setLanguage('zh-Hans');
  const owner = card();
  owner.status!.microphone = {
    mode:'source', permissionGranted:true, virtualReady:true, captureActive:false, consumerActive:false, receivingActive:false,
    sourceDevice:owner.device, sourceName:owner.name, shareAction:unavailable, localAction:unavailable,
    receiveAction:unavailable, controlsToken:'controls', forwarding:{volumePercent:100,volumeBars:3,muted:false},
    fault:{code:'backend_failed',unknownCode:null,problem:null},
  };
  let projected = presentCards(view(owner),[])[0];
  expect(projected.microphone).toMatchObject({captureActive:false,consumerActive:false,fault:{code:'backend_failed'}});
  expect(indicators(projected)[3]).toMatchObject({tone:'warn',badge:'warning'});
  owner.status!.microphone!.fault = null;
  projected = presentCards(view(owner),[])[0];
  expect(indicators(projected)[3].label).toBe('麦克风：发送');
  owner.status!.microphone!.captureActive = true;
  projected = presentCards(view(owner),[])[0];
  expect(indicators(projected)[3].label).toContain('发送');
  owner.status!.microphone!.mode = 'receiver';
  owner.status!.microphone!.captureActive = false;
  projected = presentCards(view(owner),[])[0];
  expect(indicators(projected)[3].label).toBe('麦克风：接收');
});

it.each(['preparing','sound_sleep','sleeping','paused'] as const)('speaker %s is an internal state: role badge and forwarding text stay', activity => {
  setLanguage('zh-Hans');
  const owner=card();
  Object.assign(owner.status!.speaker,{configuredMode:'source',icon:'forwarding',activity,receiverDevice:'3'.repeat(64),forwarding:{volumePercent:100,volumeBars:3,muted:false}});
  const projected=presentCards(view(owner),[])[0];
  expect(indicators(projected)[2].label).toContain('正在转发');
  // The role badge stays; activity is described in the label/tooltip only.
  expect(indicators(projected)[2].badge).toBe('send');
});
