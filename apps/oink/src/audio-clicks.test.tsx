// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { App } from './App';
import { toast } from 'oink-ui';
import { agent } from './api/store';
import type { DeviceCardsView } from './api/cards';
import { layoutProfilesFixtureSnapshot } from './api/layoutProfiles.fixture';
import { profileHarness } from './arrange/profileHarness';
import { setLanguage } from './i18n';
import { captureNotices } from './api/notices.fixture';
const notices = captureNotices();

const backend = vi.hoisted(() => ({
  harness:null as ReturnType<typeof profileHarness>|null,
  snapshot:null as DeviceCardsView|null,
  writes:[] as {method:string;params:Record<string,unknown>}[],
  failWrites:false,stallRefresh:false,refuseToken:null as string|null,
}));
vi.mock('./api/store', async original => ({...await original<typeof import('./api/store')>(), start:vi.fn()}));
vi.mock('./api/transport', async original => ({
  ...await original<typeof import('./api/transport')>(),
  transport:{
    call:async(request:{method:string;params?:Record<string,unknown>})=>{
      if(request.method==='device_cards')return backend.stallRefresh?new Promise(()=>{}):backend.snapshot;
      if(['invoke_card','invoke_device_card_action','set_exit_volume','set_forwarding_controls','lock_focus_to'].includes(request.method)){
        backend.writes.push({method:request.method,params:request.params!});
        if(backend.failWrites)throw new Error('connection lost');
        if(request.params?.token===backend.refuseToken)return {type:'device_control_completed',data:{requestId:request.params!.requestId,result:{outcome:'refused',reason:'not_exit',unknownCode:null}}};
        if(request.method==='lock_focus_to')return null;
        return {type:'device_control_accepted',data:{requestId:request.params!.requestId}};
      }
      return backend.harness!.call(request);
    },
    shell:async()=>null,onEvent:async()=>()=>{},
  },
}));
const A='a'.repeat(64);
function cards(speaker:'unused'|'forwarding'|'exit',mic:'source'|'receiver'|'muted'|'idle',muted=false,revision=1,localMuted=false):DeviceCardsView {
  const action=(name:string)=>({kind:'invoke',command:'x',token:name+':'+revision} as const);
  const resource={configuredMode:'shared',icon:'shared',fault:{code:'none',unknownCode:null,problem:null},condition:'ready',action:action('generic')} as const;
  return {
    schema:1,stream:'clicks',revision:String(revision),group:null,generatedAtMs:0,emergencyDelivery:null,
    exitVolume:speaker==='exit'?{receiverDevice:A,receiverName:'Alpha',outputName:'Speakers',volumePercent:70,muted,condition:'ready',versionToken:'exit-volume:'+revision,canSet:true,refusal:null}:null,
    cards:[{device:A,name:'Alpha',os:'macos',logo:'macos',isSelf:true,section:'this_device',lastSeenMs:null,pause:null,connection:{kind:'self'},status:{
      actions:{screenShared:action('screen'),screenLocal:action('screen'),mouseShared:action('mouse'),mouseLocal:action('mouse'),keyboardShared:action('key'),keyboardLocal:action('key'),keyboardDisabled:action('key'),keyboardEnabled:action('key'),clipboardShared:action('clip'),clipboardLocal:action('clip'),forwardingEnabled:action('forward'),forwardingDisabled:action('local')},
      cursor:{icon:'local',focusDevice:A,focusName:'Alpha',lockedDevice:null,context:'group'},
      screen:resource,mouse:resource,keyboard:{...resource,disabled:false},clipboard:resource,
      speaker:{...resource,configuredMode:speaker==='exit'?'receiver':speaker==='unused'?'local':'source',icon:speaker,action:action('leave-exit'),makeExitAction:action('exit'),forwardingControlsToken:'forward-volume:'+revision,forwardingEnabled:true,activity:'idle',receiverDevice:speaker==='exit'?A:null,receiverName:null,activeSources:0,receiveReady:true,forwarding:{volumePercent:80,muted},localOutput:{volumePercent:70,muted:speaker==='exit'?muted:localMuted}},
      microphone:{mode:mic,permissionGranted:true,virtualReady:true,captureActive:mic==='source',consumerActive:mic==='receiver',receivingActive:mic==='receiver',fault:null,sourceDevice:mic==='source'?A:mic==='receiver'?'b'.repeat(64):null,sourceName:null,shareAction:action('share'),localAction:action('receive'),receiveAction:action('receive-mute'),controlsToken:'mic-volume:'+revision,forwarding:{volumePercent:100,muted}},
      game:{preference:'auto',active:false,detected:false,actions:{auto:action('game'),on:action('game'),off:action('game')}},
    }}],
  } as unknown as DeviceCardsView;
}
beforeAll(()=>{
  vi.stubGlobal('ResizeObserver',class {observe(){} unobserve(){} disconnect(){}});
  vi.stubGlobal('PointerEvent',MouseEvent);
  Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})});
});
beforeEach(()=>{
  setLanguage('en');
  backend.writes=[];backend.failWrites=false;backend.stallRefresh=false;backend.refuseToken=null;
  backend.harness=profileHarness({snapshot:layoutProfilesFixtureSnapshot()});
  agent.adopt('boot',{platform:'macos',version:'0.1.0',build:null,backend:'agent'});
  agent.adopt('layout',backend.harness.fixture.layout());
});
afterEach(()=>{cleanup();toast.dismiss();backend.harness?.stop();agent.reset();});
function mount(sp:'unused'|'forwarding'|'exit',mic:'source'|'receiver'|'muted'|'idle',muted=false){
  backend.snapshot=cards(sp,mic,muted);
  agent.adopt('cards',backend.snapshot);
  return render(<App/>);
}
async function finish(index:number,snapshot:DeviceCardsView){
  backend.snapshot=snapshot;
  await act(async()=>{
    agent.receive({type:'device_cards',data:snapshot});
    agent.receive({type:'device_control_completed',data:{requestId:String(backend.writes[index].params.requestId),result:{outcome:'applied'}}});
  });
}
it.each([false,true])('speaker forwarding (muted=%s) mutes on click and stops sharing on double-click',async muted=>{
  const {container}=mount('forwarding','receiver',muted);
  const icon=()=>container.querySelector<HTMLButtonElement>('.dev-ico[aria-label^="Audio output"]')!;
  fireEvent.click(icon(),{detail:1});
  expect(backend.writes).toHaveLength(1);
  expect(backend.writes[0]).toMatchObject({method:'set_forwarding_controls',params:{muted:!muted}});
  fireEvent.click(icon(),{detail:2});
  fireEvent.doubleClick(icon(),{detail:2});
  expect(icon().getAttribute('aria-label')).toContain('local');
  await finish(0,cards('forwarding','receiver',!muted,2));
  expect(backend.writes).toHaveLength(2);
  expect(backend.writes[1].params.token).toBe('local:2');
  await finish(1,cards('unused','receiver',false,3));
  expect(backend.writes).toHaveLength(2);
  expect(icon().getAttribute('aria-label')).toContain('local');
});
it.each((['unused','exit'] as const).flatMap(mode=>[false,true].map(muted=>({mode,muted}))))('speaker $mode (muted=$muted) sends on first click and double-click lands last over delayed receipts',async ({mode,muted})=>{
  const {container}=mount(mode,'receiver',muted);
  const icon=()=>container.querySelector<HTMLButtonElement>('.dev-ico[aria-label^="Audio output"]')!;
  fireEvent.click(icon(),{detail:1});
  expect(backend.writes).toHaveLength(1);
  const first=backend.writes[0];
  if(mode==='exit')expect(first).toMatchObject({method:'set_exit_volume',params:{muted:!muted}});
  else expect(first.params.token).toBe((mode==='unused'?'forward':'local')+':1');
  fireEvent.click(icon(),{detail:2});
  fireEvent.doubleClick(icon(),{detail:2});
  expect(backend.writes).toHaveLength(1);
  expect(!!icon().querySelector('[data-badge="mute"]')).toBe(mode==='exit'&&!muted);
  const finalText=mode==='exit'?'local':'group output';
  expect(icon().getAttribute('aria-label')).toContain(finalText);
  await finish(0,cards(mode==='exit'?'exit':mode==='unused'?'forwarding':'unused','receiver',mode==='exit'?!muted:muted,2));
  expect(backend.writes).toHaveLength(2);
  expect(backend.writes[1].params.token).toBe((mode==='exit'?'leave-exit':'exit')+':2');
  expect(icon().getAttribute('aria-label')).toContain(finalText);
  await finish(1,cards(mode==='exit'?'unused':'exit','receiver',false,3,mode==='exit'&&!muted));
  expect(backend.writes).toHaveLength(2);
  expect(icon().getAttribute('aria-label')).toContain(finalText);
  expect(!!icon().querySelector('[data-badge="mute"]')).toBe(mode==='exit'&&!muted);
});
it.each([false,true])('microphone source (muted=%s) toggles mute on click and stops sharing on double-click',async muted=>{
  const {container}=mount('forwarding','source',muted);
  const icon=()=>container.querySelectorAll<HTMLButtonElement>('.dev-ico')[3];
  expect(!!icon().querySelector('[data-badge="mute"]')).toBe(muted);
  fireEvent.click(icon(),{detail:1});
  expect(backend.writes).toHaveLength(1);
  expect(backend.writes[0]).toMatchObject({method:'set_forwarding_controls',params:{muted:!muted}});
  fireEvent.click(icon(),{detail:2});
  fireEvent.doubleClick(icon(),{detail:2});
  expect(icon().getAttribute('aria-label')).toContain('Microphone: Local');
  await finish(0,cards('forwarding','source',!muted,2));
  expect(backend.writes).toHaveLength(2);
  expect(backend.writes[1].params.token).toBe('receive:2');
  expect(icon().getAttribute('aria-label')).toContain('Microphone: Local');
  // Release first enables receiving in v1; Local must execute the second setter.
  await finish(1,cards('forwarding','idle',!muted,3));
  expect(backend.writes).toHaveLength(3);
  expect(backend.writes[2].params.token).toBe('receive:3');
  await finish(2,cards('forwarding','muted',!muted,4));
  expect(agent.get().cards?.cards[0].status?.microphone?.mode).toBe('muted');
  expect(icon().getAttribute('aria-label')).toContain('Microphone: Local');
  expect(backend.writes).toHaveLength(3);
  expect(notices()).toEqual([]);
});
it.each(['idle','receiver','muted'] as const)('microphone %s sends mode immediately and double-click mode last',async mode=>{
  const {container}=mount('forwarding',mode);
  const icon=()=>container.querySelectorAll<HTMLButtonElement>('.dev-ico')[3];
  fireEvent.click(icon(),{detail:1});
  expect(backend.writes).toHaveLength(1);
  expect(backend.writes[0].params.token).toBe((mode==='muted'?'receive-mute':'receive')+':1');
  fireEvent.click(icon(),{detail:2});
  fireEvent.doubleClick(icon(),{detail:2});
  expect(icon().getAttribute('aria-label')).toContain('Microphone: Send');
  await finish(0,cards('forwarding',mode==='muted'?'receiver':'muted',false,2));
  expect(backend.writes).toHaveLength(2);
  expect(backend.writes[1].params.token).toBe('share:2');
  await finish(1,cards('forwarding','source',false,3));
  expect(icon().getAttribute('aria-label')).toContain('Microphone: Send');
  expect(agent.get().cards?.cards[0].status?.microphone?.mode).toBe('source');
});

it.each(['idle','unavailable'] as const)('missing virtual microphone blocks receive from %s before dispatch and offers installation',async mode=>{
  backend.snapshot=cards('unused','idle');
  const mic=backend.snapshot.cards[0].status!.microphone!;
  mic.mode=mode;
  mic.virtualReady=false;
  mic.receiveAction={kind:'unavailable',reason:'state_unknown'} as typeof mic.receiveAction;
  agent.adopt('cards',backend.snapshot);
  const {container}=render(<App/>);
  const icon=container.querySelectorAll<HTMLButtonElement>('.dev-ico')[3];
  fireEvent.click(icon,{detail:1});
  await act(async()=>{});
  expect(backend.writes).toHaveLength(0);
  expect(notices()).toEqual([]);
  expect(document.body.textContent).toContain('Virtual microphone is not installed');
});

it('a remote device without a virtual microphone names that device and offers no install button',async()=>{
  backend.snapshot=cards('unused','idle');
  const card=backend.snapshot.cards[0];
  card.isSelf=false;card.section='online' as typeof card.section;card.connection={kind:'direct'} as unknown as typeof card.connection;
  card.status!.microphone!.virtualReady=false;
  agent.adopt('cards',backend.snapshot);
  const {container}=render(<App/>);
  fireEvent.click(container.querySelectorAll<HTMLButtonElement>('.dev-ico')[3],{detail:1});
  await act(async()=>{});
  expect(backend.writes).toHaveLength(0);
  const notice=screen.getByText('Install the virtual microphone on Alpha').closest('.ok-toast');
  expect(notice).not.toBeNull();
  expect(notice!.querySelector('.ok-toast__action')).toBeNull();
});

it('receive needs no capture permission; send needs no virtual microphone',async()=>{
  backend.snapshot=cards('unused','muted');
  backend.snapshot.cards[0].status!.microphone!.permissionGranted=false;
  agent.adopt('cards',backend.snapshot);
  const {container}=render(<App/>);
  const icon=()=>container.querySelectorAll<HTMLButtonElement>('.dev-ico')[3];
  fireEvent.click(icon(),{detail:1});
  expect(backend.writes[0].params.token).toBe('receive-mute:1');
  await finish(0,cards('unused','receiver',false,2));
  backend.snapshot=cards('unused','muted',false,3);
  backend.snapshot.cards[0].status!.microphone!.virtualReady=false;
  await act(async()=>{agent.adopt('cards',backend.snapshot);});
  fireEvent.keyDown(icon(),{key:'Enter',shiftKey:true});
  expect(backend.writes[1].params.token).toBe('share:3');
  expect(notices()).toEqual([]);
});

it('missing capture permission blocks Send with an actionable toast',async()=>{
  backend.snapshot=cards('unused','muted');
  backend.snapshot.cards[0].status!.microphone!.permissionGranted=false;
  agent.adopt('cards',backend.snapshot);
  const {container}=render(<App/>);
  fireEvent.keyDown(container.querySelectorAll('.dev-ico')[3],{key:'Enter',shiftKey:true});
  await act(async()=>{});
  expect(backend.writes).toHaveLength(0);
  expect(notices()).toEqual([]);
  expect(document.body.textContent).toContain('Microphone permission has not been granted');
  expect(document.querySelector('.ok-toast button')?.textContent).toContain('Grant access');
});

it('keeps every control enabled and shows rapid mouse intents before receipts', async()=>{
  const {container}=mount('forwarding','source');
  const mouse=()=>container.querySelectorAll<HTMLButtonElement>('.dev-ico')[0];
  fireEvent.click(mouse(),{detail:1});
  expect(mouse().getAttribute('aria-label')).toContain('local');
  expect(container.querySelector('[aria-busy="true"]')).toBeNull();
  expect(container.querySelector<HTMLButtonElement>('.vol__btn')!.disabled).toBe(false);
  fireEvent.click(mouse(),{detail:1});
  expect(mouse().getAttribute('aria-label')).toContain('shared');
  // A newer report is the rollback baseline, never a reason to erase this intent.
  const snapshot=cards('forwarding','source',false,2);
  snapshot.cards[0].status!.mouse={...snapshot.cards[0].status!.mouse,configuredMode:'local',icon:'local'};
  await finish(0,snapshot);
  expect(mouse().getAttribute('aria-label')).toContain('shared');
});
it('rolls audio mode back immediately on transport failure even if refreshing cards stalls',async()=>{
  const {container}=mount('forwarding','source');
  backend.failWrites=true;backend.stallRefresh=true;
  fireEvent.keyDown(container.querySelectorAll('.dev-ico')[2],{key:'Enter',shiftKey:true});
  await act(async()=>{});
  expect(container.querySelectorAll('.dev-ico')[2].getAttribute('aria-label')).toContain('Click: Mute');
  expect(notices()).toEqual([]);
});
it('times out accepted audio writes and restores the latest device report',async()=>{
  vi.useFakeTimers();
  try {
    const {container}=mount('forwarding','source');
    fireEvent.keyDown(container.querySelectorAll('.dev-ico')[2],{key:'Enter',shiftKey:true});
    expect(container.querySelectorAll('.dev-ico')[2].getAttribute('aria-label')).toContain('local');
    await act(async()=>{await vi.advanceTimersByTimeAsync(10_001);});
    expect(container.querySelectorAll('.dev-ico')[2].getAttribute('aria-label')).toContain('Click: Mute');
    expect(notices()).toEqual([]);
  } finally {vi.useRealTimers();}
});

it('keeps Local visible through both v1 release and receiving-disable steps',async()=>{
  const {container}=mount('forwarding','source');
  const icon=()=>container.querySelectorAll('.dev-ico')[3];
  fireEvent.keyDown(icon(),{key:'Enter',shiftKey:true});
  expect(icon().getAttribute('aria-label')).toContain('Microphone: Local');
  await finish(0,cards('forwarding','idle',false,2));
  expect(backend.writes).toHaveLength(2);
  expect(backend.writes[1].params.token).toBe('receive:2');
  expect(icon().getAttribute('aria-label')).toContain('Microphone: Local');
  await finish(1,cards('forwarding','muted',false,3));
  expect(icon().getAttribute('aria-label')).toContain('Microphone: Local');
});

it('keeps the menu Segmented controls enabled and selected through rapid intents',async()=>{
  const {container}=mount('forwarding','source');
  // Leave a mouse action outstanding while the keyboard and microphone remain usable.
  fireEvent.click(container.querySelectorAll('.dev-ico')[0],{detail:1});
  fireEvent.contextMenu(container.querySelector('.dev')!);
  fireEvent.click(await screen.findByRole('menuitem',{name:'Input devices'}));
  const keyboard=await screen.findByRole('radiogroup',{name:'Keyboard'});
  const local=within(keyboard).getByRole('radio',{name:/^Local/});
  fireEvent.click(local);
  expect(local.getAttribute('aria-checked')).toBe('true');
  expect((local as HTMLButtonElement).disabled).toBe(false);
  const shared=within(keyboard).getByRole('radio',{name:/^Shared/});
  fireEvent.click(shared);
  expect(shared.getAttribute('aria-checked')).toBe('true');
  expect((shared as HTMLButtonElement).disabled).toBe(false);
  const microphone=screen.getByRole('radiogroup',{name:'Microphone'});
  fireEvent.click(within(microphone).getByRole('radio',{name:'Local'}));
  expect(within(microphone).getByRole('radio',{name:'Local'}).getAttribute('aria-checked')).toBe('true');
  expect(backend.writes).toHaveLength(3); // mouse, keyboard, microphone: independent sends
  expect(container.querySelector('[aria-busy="true"]')).toBeNull();
});
it('a lock click is immediately undoable before the owner reports it',async()=>{
  const {container}=mount('forwarding','source');
  fireEvent.contextMenu(container.querySelector('.dev')!);
  fireEvent.click(await screen.findByRole('menuitem',{name:'Lock pointer'}));
  expect(backend.writes[0].method).toBe('lock_focus_to');
  expect(container.querySelector('.dev__hit')?.getAttribute('aria-label')).toContain('pointer locked here');
  fireEvent.contextMenu(container.querySelector('.dev')!);
  fireEvent.click(await screen.findByRole('menuitem',{name:'Unlock pointer'}));
  expect(container.querySelector('.dev__hit')?.getAttribute('aria-label')).not.toContain('pointer locked here');
  const snapshot=cards('forwarding','source',false,2);
  snapshot.cards[0].status!.cursor={...snapshot.cards[0].status!.cursor,icon:'locked',lockedDevice:A};
  await act(async()=>{backend.snapshot=snapshot;agent.receive({type:'device_cards',data:snapshot});});
  expect(backend.writes).toHaveLength(2);
  expect(backend.writes[1].params.token).toBe('screen:2');
  expect(container.querySelector('.dev__hit')?.getAttribute('aria-label')).not.toContain('pointer locked here');
});
it('rolls a failed mouse intent back to a newer confirmed report, not its pre-click value',async()=>{
  const {container}=mount('forwarding','source');
  fireEvent.click(container.querySelectorAll('.dev-ico')[0],{detail:1});
  const snapshot=cards('forwarding','source',false,2);
  snapshot.cards[0].status!.mouse.icon='disabled';
  await act(async()=>{backend.snapshot=snapshot;agent.receive({type:'device_cards',data:snapshot});});
  expect(container.querySelectorAll('.dev-ico')[0].getAttribute('aria-label')).toContain('Mouse: local');
  await act(async()=>{agent.receive({type:'device_control_completed',data:{requestId:String(backend.writes[0].params.requestId),result:{outcome:'failed',reason:'link_lost',unknownCode:null}}});});
  expect(container.querySelectorAll('.dev-ico')[0].getAttribute('aria-label')).toContain('Mouse: disabled');
  expect(notices()).toContain('link_lost');
});
it('mute and volume stay enabled and the newer volume unmutes over an earlier mute receipt',async()=>{
  const {container}=mount('exit','source');
  fireEvent.click(container.querySelector('.dev__hit')!);
  const mute=container.querySelector<HTMLButtonElement>('.vol__btn')!;
  const glyph=mute.querySelector('.vol__glyph');
  fireEvent.click(mute);
  expect(mute.disabled).toBe(false);expect(mute.getAttribute('aria-pressed')).toBe('true');
  const slider=container.querySelector('[role="slider"]')!;
  fireEvent.keyDown(slider,{key:'ArrowRight'});
  expect(slider.getAttribute('aria-valuenow')).toBe('71');
  expect(mute.getAttribute('aria-pressed')).toBe('false');
  await finish(0,cards('exit','source',true,2));
  // Volume spacing may defer the wire write, never its presentation.
  expect(slider.getAttribute('aria-valuenow')).toBe('71');
  expect(mute.getAttribute('aria-pressed')).toBe('false');
  expect(mute.querySelector('.vol__glyph')).toBe(glyph);expect(mute.disabled).toBe(false);
});

it('Local still disables receiving after releasing Send without a virtual microphone',async()=>{
  backend.snapshot=cards('forwarding','source');
  backend.snapshot.cards[0].status!.microphone!.virtualReady=false;
  agent.adopt('cards',backend.snapshot);
  const {container}=render(<App/>);
  fireEvent.keyDown(container.querySelectorAll('.dev-ico')[3],{key:'Enter',shiftKey:true});
  const released=cards('forwarding','idle',false,2);
  released.cards[0].status!.microphone!.mode='unavailable';
  released.cards[0].status!.microphone!.virtualReady=false;
  await finish(0,released);
  expect(backend.writes).toHaveLength(2);
  expect(backend.writes[1].params.token).toBe('receive:2');
  expect(container.querySelectorAll('.dev-ico')[3].getAttribute('aria-label')).toContain('Microphone: Local');
});
it('shows game-mode changes immediately and restores the confirmed mode on refusal',async()=>{
  const {container}=mount('forwarding','source');
  fireEvent.contextMenu(container.querySelector('.dev')!);
  fireEvent.click(await screen.findByRole('menuitem',{name:'Game mode'}));
  fireEvent.click(await screen.findByRole('menuitemradio',{name:'On'}));
  expect(container.querySelector('.dev__hit')?.getAttribute('aria-label')).toContain('game mode on');
  expect(backend.writes).toHaveLength(1);
  await act(async()=>{agent.receive({type:'device_control_completed',data:{requestId:String(backend.writes[0].params.requestId),result:{outcome:'refused',reason:'apply_failed',unknownCode:null}}});});
  expect(container.querySelector('.dev__hit')?.getAttribute('aria-label')).not.toContain('game mode on');
  expect(notices()).toContain('apply_failed');
});
it('a new explicit keyboard mode finishes enable then configure without showing the old mode',async()=>{
  backend.snapshot=cards('forwarding','source');
  backend.snapshot.cards[0].status!.keyboard={...backend.snapshot.cards[0].status!.keyboard,configuredMode:'local',icon:'disabled',disabled:true};
  agent.adopt('cards',backend.snapshot);
  const {container}=render(<App/>);
  fireEvent.contextMenu(container.querySelector('.dev')!);
  fireEvent.click(await screen.findByRole('menuitem',{name:'Input devices'}));
  const keyboard=await screen.findByRole('radiogroup',{name:'Keyboard'});
  fireEvent.click(within(keyboard).getByRole('radio',{name:/^Shared/}));
  const enabled=cards('forwarding','source',false,2);
  enabled.cards[0].status!.keyboard={...enabled.cards[0].status!.keyboard,configuredMode:'local',icon:'local'};
  await finish(0,enabled);
  expect(backend.writes).toHaveLength(2);
  expect(within(keyboard).getByRole('radio',{name:/^Shared/}).getAttribute('aria-checked')).toBe('true');
  await finish(1,cards('forwarding','source',false,3));
  expect(within(keyboard).getByRole('radio',{name:/^Shared/}).getAttribute('aria-checked')).toBe('true');
});
it('does not apply a queued output volume after making an output fails',async()=>{
  const {container}=mount('unused','source');
  fireEvent.keyDown(container.querySelectorAll('.dev-ico')[2],{key:'Enter',shiftKey:true});
  const slider=container.querySelector('[role="slider"]')!;
  fireEvent.keyDown(slider,{key:'ArrowRight'});
  expect(backend.writes).toHaveLength(1);
  await act(async()=>{agent.receive({type:'device_control_completed',data:{requestId:String(backend.writes[0].params.requestId),result:{outcome:'failed',reason:'apply_failed',unknownCode:null}}});});
  expect(backend.writes).toHaveLength(1); // the output-only intent is dropped with the failed output
});

it.each((['keyboard','mouse'] as const).flatMap(kind=>(['shared','local','disabled'] as const).map(mode=>({kind,mode}))))('$kind $mode clicks apply immediately and double click ends disabled unless restoring',async({kind,mode})=>{
  // A disabled input only restores: its double-click must end shared, not disabled again.
  const restoring=mode==='disabled';
  backend.snapshot=cards('unused','receiver');
  const status=backend.snapshot.cards[0].status!;
  status.actions.mouseDisabled={kind:'invoke',command:'set_local_option',token:'mouse-disable:1'};
  status[kind].configuredMode=mode==='disabled'?'local':mode;
  status[kind].icon=mode;
  if(kind==='keyboard')status.keyboard.disabled=mode==='disabled';
  agent.adopt('cards',backend.snapshot);
  const {container}=render(<App/>);
  const icon=()=>container.querySelector<HTMLButtonElement>(`.dev-ico[data-glyph="${kind}"]`)!;
  fireEvent.click(icon(),{detail:1});
  expect(icon().getAttribute('aria-label')).toContain(mode==='shared'?': local':': shared');
  expect(backend.writes).toHaveLength(1);
  expect(container.querySelector('.dev__hit[aria-current="true"]')).not.toBeNull();
  fireEvent.click(icon(),{detail:2});
  expect(icon().getAttribute('aria-label')).toContain(restoring?': shared':': disabled');
  const report=structuredClone(backend.snapshot);
  report.revision='2';
  report.cards[0].status![kind].configuredMode=mode==='shared'?'local':'shared';
  report.cards[0].status![kind].icon=mode==='shared'?'local':'shared';
  if(kind==='keyboard')report.cards[0].status!.keyboard.disabled=false;
  await finish(0,report);
  expect(icon().getAttribute('aria-label')).toContain(restoring?': shared':': disabled');
  expect(backend.writes).toHaveLength(restoring?1:2);
});
it('mouse double click without disable capability only performs the immediate single click',()=>{
  const {container}=mount('unused','receiver');
  const icon=container.querySelector('.dev-ico[data-glyph="mouse"]')!;
  fireEvent.click(icon,{detail:1});fireEvent.click(icon,{detail:2});
  expect(backend.writes).toHaveLength(1);
  expect(icon.getAttribute('aria-label')).toContain(': local');
  expect(icon.getAttribute('aria-label')).not.toContain('Double-click');
});
it('a disabled keyboard single click restores shared after enabling its previously local mode',async()=>{
  backend.snapshot=cards('unused','receiver');
  backend.snapshot.cards[0].status!.keyboard={...backend.snapshot.cards[0].status!.keyboard,configuredMode:'local',icon:'disabled',disabled:true};
  agent.adopt('cards',backend.snapshot);
  const {container}=render(<App/>);
  const icon=()=>container.querySelector<HTMLButtonElement>('.dev-ico[data-glyph="keyboard"]')!;
  fireEvent.click(icon(),{detail:1});
  expect(icon().getAttribute('aria-label')).toContain(': shared');
  expect(backend.writes).toHaveLength(1);
  const enabled=cards('unused','receiver',false,2);
  enabled.cards[0].status!.keyboard={...enabled.cards[0].status!.keyboard,configuredMode:'local',icon:'local'};
  await finish(0,enabled);
  expect(backend.writes).toHaveLength(2);
  expect(icon().getAttribute('aria-label')).toContain(': shared');
  await finish(1,cards('unused','receiver',false,3));
  expect(icon().getAttribute('aria-label')).toContain(': shared');
});

it.each(['macos','windows'] as const)('allows Receive with no source on %s and shows a warning without an error',async platform=>{
  agent.adopt('boot',{platform,version:'0.1.0',build:null,backend:'agent'});
  const {container}=mount('unused','muted');
  const icon=()=>container.querySelector('[data-glyph="microphone"]')!;
  fireEvent.click(icon(),{detail:1});
  expect(backend.writes[0].params.token).toBe('receive-mute:1');
  await finish(0,cards('unused','idle',false,2));
  expect(icon().getAttribute('aria-label')).toContain('No device is currently sharing a microphone');
  expect(icon().querySelector('[data-badge="warning"]')).not.toBeNull();
  expect(notices()).toEqual([]);
});

it.each(['macos','windows'] as const)('reports a microphone owner refusal in a toast on %s, never the backend notice',async platform=>{
  agent.adopt('boot',{platform,version:'0.1.0',build:null,backend:'agent'});
  const {container}=mount('unused','muted');
  fireEvent.keyDown(container.querySelector('[data-glyph="microphone"]')!,{key:'Enter',shiftKey:true});
  await act(async()=>{agent.receive({type:'device_control_completed',data:{requestId:String(backend.writes[0].params.requestId),result:{outcome:'refused',reason:'apply_failed',unknownCode:null}}});});
  expect(document.querySelector('.ok-toast')?.textContent).toContain('Could not change microphone settings');
  expect(document.querySelector('.ok-toast')?.textContent).toContain('Could not apply setting');
  expect(notices()).toEqual([]);
});
it('explains unavailable microphone facts instead of falsely reporting unsupported',async()=>{
  backend.snapshot=cards('unused','muted');
  backend.snapshot.cards[0].status!.microphone!.receiveAction={kind:'unavailable',reason:'state_unknown'};
  agent.adopt('cards',backend.snapshot);
  const {container}=render(<App/>);
  fireEvent.click(container.querySelector('[data-glyph="microphone"]')!,{detail:1});
  await act(async()=>{});
  expect(backend.writes).toHaveLength(0);
  expect(document.querySelector('.ok-toast')?.textContent).toContain('State changed; try again');
  expect(notices()).toEqual([]);
});
it('microphone transport failure uses a toast even when refresh stalls',async()=>{
  const {container}=mount('unused','muted');backend.failWrites=true;backend.stallRefresh=true;
  fireEvent.click(container.querySelector('[data-glyph="microphone"]')!,{detail:1});
  await act(async()=>{});
  expect(document.querySelector('.ok-toast')?.textContent).toContain('Could not change microphone settings');
  expect(notices()).toEqual([]);
  expect(container.querySelector('[data-glyph="microphone"]')?.getAttribute('aria-label')).toContain('Microphone: Local');
});

it('does not release Send twice when its preference revision arrives before its mode',async()=>{
  const {container}=mount('forwarding','source');
  // This is the old Send double-click sequence (Local then Receive), expressed
  // through today's stop-sharing gesture and explicit Receive menu choice.
  fireEvent.keyDown(container.querySelector('[data-glyph="microphone"]')!,{key:'Enter',shiftKey:true});
  fireEvent.contextMenu(container.querySelector('.dev')!);
  fireEvent.click(await screen.findByRole('menuitem',{name:'Input devices'}));
  fireEvent.click(within(await screen.findByRole('radiogroup',{name:'Microphone'})).getByRole('radio',{name:'Receive'}));
  expect(backend.writes).toHaveLength(1);
  // Release has persisted; the audio status projection still says Source.
  // Releasing again with its *new* option revision returns NotExit at the owner.
  backend.refuseToken='receive:2';
  await finish(0,cards('forwarding','source',false,2));
  const released=cards('forwarding','idle',false,3);
  backend.snapshot=released;
  await act(async()=>{agent.receive({type:'device_cards',data:released});});
  expect(document.querySelector('.ok-toast:not([data-leaving])')?.textContent??'').not.toContain('Could not change microphone settings');
  expect(backend.writes.map(w=>w.params.token)).toEqual(['receive:1']);
  expect(container.querySelector('[data-glyph="microphone"]')?.getAttribute('aria-label')).toContain('No device is currently sharing a microphone');
});

const micModes=['muted','receiver','source'] as const;
type MicMode=typeof micModes[number];
async function chooseMicrophone(container:HTMLElement,mode:MicMode){
  fireEvent.contextMenu(container.querySelector('.dev')!);
  fireEvent.click(await screen.findByRole('menuitem',{name:'Input devices'}));
  fireEvent.click(within(await screen.findByRole('radiogroup',{name:'Microphone'})).getByRole('radio',{name:{muted:'Local',receiver:'Receive',source:'Send'}[mode]}));
}
async function reportMicrophone(mode:MicMode|'idle',revision:number){
  backend.snapshot=cards('forwarding',mode,false,revision);
  await act(async()=>{agent.receive({type:'device_cards',data:backend.snapshot!});});
}
async function microphoneReceipt(index:number,outcome:'applied'|'no_change'='applied'){
  await act(async()=>{agent.receive({type:'device_control_completed',data:{requestId:String(backend.writes[index].params.requestId),result:{outcome}}});});
}
it.each(micModes.flatMap(from=>micModes.filter(to=>to!==from).flatMap(to=>[false,true].map(receiptFirst=>({from,to,receiptFirst})))))('menu microphone $from → $to waits for mode facts (receipt first=$receiptFirst)',async({from,to,receiptFirst})=>{
  const {container}=mount('forwarding',from);
  await chooseMicrophone(container,to);
  expect(backend.writes).toHaveLength(1);
  if(receiptFirst)await microphoneReceipt(0);
  await reportMicrophone(from,2); // changed option token, old resolved mode
  if(!receiptFirst)await microphoneReceipt(0);
  expect(backend.writes).toHaveLength(1);
  if(from==='source'&&to==='muted'){
    await reportMicrophone('idle',3);
    expect(backend.writes.map(w=>w.params.token)).toEqual(['receive:1','receive:3']);
    await finish(1,cards('forwarding','muted',false,4));
  }else await reportMicrophone(to,3);
  expect(container.querySelector('[data-glyph="microphone"]')?.getAttribute('aria-label')).toContain('Microphone: '+{muted:'Local',receiver:'Receive',source:'Send'}[to]);
  expect(document.querySelector('.ok-toast:not([data-leaving])')).toBeNull();
  expect(notices()).toEqual([]);
});
it.each(micModes.flatMap(from=>micModes.filter(first=>first!==from).flatMap(first=>(['applied','no_change'] as const).map(outcome=>({from,first,outcome})))))('superseded $from → $first → $from waits for resolved facts after $outcome',async({from,first,outcome})=>{
  const {container}=mount('forwarding',from);
  await chooseMicrophone(container,first);
  await chooseMicrophone(container,from);
  await reportMicrophone(from,2);
  await microphoneReceipt(0,outcome);
  expect(backend.writes).toHaveLength(1); // never plan from the stale source/mode
  // Source release enables Receive before the optional Local disable step.
  const actual=from==='source'?'receiver':first;
  await reportMicrophone(actual,3);
  expect(backend.writes).toHaveLength(2);
  const last=from==='source'?'share:3':actual==='source'?'receive:3':from==='muted'?'receive:3':'receive-mute:3';
  expect(backend.writes[1].params.token).toBe(last);
  if(actual==='source'&&from==='muted'){
    await finish(1,cards('forwarding','idle',false,4));
    expect(backend.writes[2].params.token).toBe('receive:4');
    await finish(2,cards('forwarding','muted',false,5));
  }else await finish(1,cards('forwarding',from,false,4));
  expect(document.querySelector('.ok-toast:not([data-leaving])')).toBeNull();
  expect(notices()).toEqual([]);
});
it('still reports an actual microphone release refusal rather than suppressing not_exit',async()=>{
  const {container}=mount('forwarding','source');backend.refuseToken='receive:1';
  await chooseMicrophone(container,'receiver');
  await act(async()=>{});
  expect(document.querySelector('.ok-toast:not([data-leaving])')?.textContent).toContain('Could not change microphone settings');
  expect(backend.writes).toHaveLength(1);
  expect(container.querySelector('[data-glyph="microphone"]')?.getAttribute('aria-label')).toContain('Microphone: Send');
});

it.each(['exit','forwarding','microphone'] as const)('retains a mute after switching to %s until owner controls arrive',async target=>{
  const mic=target==='microphone';
  const {container}=mount('unused','receiver');
  if(mic)await chooseMicrophone(container,'source');
  else if(target==='exit')fireEvent.keyDown(container.querySelectorAll('.dev-ico')[2],{key:'Enter',shiftKey:true});
  else fireEvent.click(container.querySelector('[data-glyph="speaker"]')!);
  // Optimistic mode permits a mute before the mode receipt/control facts arrive.
  fireEvent.click(container.querySelector(`[data-glyph="${mic?'microphone':'speaker'}"]`)!);
  expect(backend.writes).toHaveLength(1);
  const mode=target==='exit'?'exit':'forwarding';
  const gap=cards(mode,mic?'source':'receiver',false,2);
  if(mic){gap.cards[0].status!.microphone!.controlsToken=null;gap.cards[0].status!.microphone!.forwarding=null;}
  else if(target==='exit'){gap.exitVolume!.canSet=false;gap.exitVolume!.versionToken=null;gap.exitVolume!.volumePercent=null;gap.exitVolume!.muted=null;gap.exitVolume!.condition='state_unknown';}
  else {gap.cards[0].status!.speaker.forwardingControlsToken=null;gap.cards[0].status!.speaker.forwarding=null;}
  await finish(0,gap);
  expect(notices()).toEqual([]);
  expect(backend.writes).toHaveLength(1);
  backend.snapshot=cards(mode,mic?'source':'receiver',false,3);
  await act(async()=>{agent.receive({type:'device_cards',data:backend.snapshot!});});
  expect(backend.writes).toHaveLength(2);
  expect(backend.writes[1]).toMatchObject({method:target==='exit'?'set_exit_volume':'set_forwarding_controls',params:{versionToken:(mic?'mic-volume':target==='exit'?'exit-volume':'forward-volume')+':3',muted:true}});
  await finish(1,cards(mode,mic?'source':'receiver',true,4));
  expect(notices()).toEqual([]);
  expect(document.querySelector('.ok-toast:not([data-leaving])')).toBeNull();
});

it('waits for exit identity instead of applying the output slider to forwarding gain',async()=>{
  const {container}=mount('unused','receiver');
  fireEvent.keyDown(container.querySelectorAll('.dev-ico')[2],{key:'Enter',shiftKey:true});
  fireEvent.keyDown(container.querySelector('[role="slider"]')!,{key:'ArrowRight'});
  const gap=cards('exit','receiver',false,2);gap.exitVolume=null;
  await finish(0,gap);
  expect(backend.writes).toHaveLength(1);
  backend.snapshot=cards('exit','receiver',false,3);
  await act(async()=>{agent.receive({type:'device_cards',data:backend.snapshot!});});
  expect(backend.writes[1]).toMatchObject({method:'set_exit_volume',params:{versionToken:'exit-volume:3',volumePercent:71}});
  expect(notices()).toEqual([]);
});

it.each(['speaker','microphone'] as const)('drops a queued %s mute if an external role change invalidates it',async kind=>{
  const mic=kind==='microphone';
  const {container}=mount('exit','source');
  const icon=container.querySelector(`[data-glyph="${kind}"]`)!;
  fireEvent.click(icon);fireEvent.click(icon);
  const changed=cards('forwarding','receiver',false,2);
  if(mic){changed.cards[0].status!.microphone!.controlsToken=null;changed.cards[0].status!.microphone!.forwarding=null;}
  else {changed.cards[0].status!.speaker.forwardingControlsToken=null;changed.cards[0].status!.speaker.forwarding=null;}
  backend.snapshot=changed;
  await act(async()=>{
    agent.receive({type:'device_cards',data:changed});
    agent.receive({type:'device_control_completed',data:{requestId:String(backend.writes[0].params.requestId),result:{outcome:'refused',reason:'stale_revision',unknownCode:null}}});
  });
  expect(backend.writes).toHaveLength(1);
  expect(notices()).toEqual([]);
  expect(document.querySelector('.ok-toast:not([data-leaving])')).toBeNull();
});

it.each(['refused','failed'] as const)('silently rolls back backend volume_unsupported (%s) without retrying a native endpoint',async outcome=>{
  const {container}=mount('exit','source');
  fireEvent.click(container.querySelector('[data-glyph="speaker"]')!);
  await act(async()=>{agent.receive({type:'device_control_completed',data:{requestId:String(backend.writes[0].params.requestId),result:{outcome,reason:'volume_unsupported',unknownCode:null}}});});
  expect(backend.writes).toHaveLength(1);
  expect(container.querySelector('[data-glyph="speaker"] [data-badge="mute"]')).toBeNull();
  expect(notices()).toEqual([]);
  expect(document.querySelector('.ok-toast:not([data-leaving])')).toBeNull();
});

it.each(['speaker','microphone'] as const)('rebases a stale %s volume revision and sends the latest absolute mute',async kind=>{
  const {container}=mount('exit','source');
  fireEvent.click(container.querySelector(`[data-glyph="${kind}"]`)!);
  await act(async()=>{agent.receive({type:'device_control_completed',data:{requestId:String(backend.writes[0].params.requestId),result:{outcome:'refused',reason:'stale_revision',unknownCode:null}}});});
  expect(backend.writes).toHaveLength(1);
  backend.snapshot=cards('exit','source',false,2);
  await act(async()=>{agent.receive({type:'device_cards',data:backend.snapshot!});});
  // Mute clicks are final writes and bypass the drag spacing interval.
  expect(backend.writes[1]).toMatchObject({params:{versionToken:(kind==='speaker'?'exit-volume':'mic-volume')+':2',muted:true}});
  await finish(1,cards('exit','source',true,3));
  expect(notices()).toEqual([]);
  expect(document.querySelector('.ok-toast:not([data-leaving])')).toBeNull();
});


it('a unified disabled keyboard unlocks to shared with one frozen v1 command',async()=>{
  backend.snapshot=cards('unused','receiver');
  backend.snapshot.cards[0].status!.keyboard={...backend.snapshot.cards[0].status!.keyboard,configuredMode:'shared',icon:'disabled',disabled:true};
  agent.adopt('cards',backend.snapshot);
  const {container}=render(<App/>);
  const icon=()=>container.querySelector<HTMLButtonElement>('.dev-ico[data-glyph="keyboard"]')!;
  fireEvent.click(icon(),{detail:1});
  expect(backend.writes).toHaveLength(1);
  await finish(0,cards('unused','receiver',false,2));
  expect(backend.writes).toHaveLength(1);
  expect(icon().getAttribute('aria-label')).toContain(': shared');
});
