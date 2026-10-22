// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { App } from './App';
import { agent } from './api/store';
import type { DeviceCardsView, DeviceCardStatus, ResourceCard, SharingMode } from './api/cards';
import type { PermissionItem } from './api/types';
import { cameraStore } from './arrange/cameraStore';
import { threeComputers } from './arrange/threeComputers.fixture';
import { profileHarness } from './arrange/profileHarness';
import { setLanguage } from './i18n';

const backend = vi.hoisted(() => ({
  harness: null as null | ReturnType<typeof import('./arrange/profileHarness').profileHarness>,
  permissions: [] as PermissionItem[],
}));
vi.mock('./api/store', async original => ({ ...await original<typeof import('./api/store')>(), start: vi.fn() }));
vi.mock('./api/transport', async original => ({
  ...await original<typeof import('./api/transport')>(),
  transport: {
    call: (request: { method: string; params?: Record<string, unknown> }) => request.method === 'permissions'
      ? Promise.resolve(backend.permissions) : backend.harness!.call(request),
    shell: async () => null,
    onEvent: async () => () => {},
  },
}));
const [A, B, C] = ['a', 'b', 'c'].map(d => d.repeat(64));
const unavailable = {kind:'unavailable',reason:'state_unknown'} as const;
const resource = (): ResourceCard<SharingMode> => ({configuredMode:'shared',icon:'shared',fault:null,condition:'ready',action:unavailable});
const status = (accessibilityGranted?: boolean): DeviceCardStatus => ({
  accessibilityGranted,
  actions:{screenShared:unavailable,screenLocal:unavailable,mouseShared:unavailable,mouseLocal:unavailable,keyboardShared:unavailable,keyboardLocal:unavailable,keyboardDisabled:unavailable,keyboardEnabled:unavailable,clipboardShared:unavailable,clipboardLocal:unavailable,forwardingEnabled:unavailable,forwardingDisabled:unavailable},
  cursor:{icon:'unknown',focusDevice:null,focusName:null,lockedDevice:null,context:'unknown'},
  screen:resource(),mouse:resource(),keyboard:{...resource(),disabled:false},clipboard:resource(),
  speaker:{...resource(),configuredMode:null,icon:'unknown',makeExitAction:unavailable,forwardingControlsToken:null,localOutput:null,forwardingEnabled:null,activity:'unknown',receiverDevice:null,receiverName:null,activeSources:null,receiveReady:null,forwarding:null},
  game:{preference:null,active:null,detected:null,actions:{auto:unavailable,on:unavailable,off:unavailable}},
});
const cards = (remoteGranted?: boolean): DeviceCardsView => ({schema:1,stream:'test',revision:'1',group:null,generatedAtMs:0,exitVolume:null,emergencyDelivery:null,
  cards:[A,B,C].map(device=>({device,name:device===C?'This Mac':device===B?'Other Mac':'PC',os:device===A?'windows':'macos',logo:device===A?'windows10':'macos',
    isSelf:device===C,section:device===C?'this_device':'online',lastSeenMs:null,status:device===B?status(remoteGranted):null,pause:null,
    connection:device===C?{kind:'self'}:{kind:'online',signal:{kind:'unknown',rttMs:null,bars:0,colour:'grey'}}})),
});
beforeAll(() => {
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  vi.stubGlobal('PointerEvent', MouseEvent);
  HTMLElement.prototype.setPointerCapture = () => {};
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(() => ({ left:0,top:0,x:0,y:0,width:900,height:600,right:900,bottom:600,toJSON(){} }));
  Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})});
});
beforeEach(() => {
  setLanguage('en');
  backend.permissions=[];
  agent.adopt('boot',{platform:'macos',version:'0.1.0',build:null,backend:'agent'});
  agent.adopt('cards',cards());
  agent.adopt('layout',threeComputers);
  backend.harness=profileHarness({layout:threeComputers});
});
afterEach(() => {backend.harness!.stop();cleanup();agent.reset();localStorage.clear();});

it('removes every remote AX-missing screen from rendering and selection, then restores it without saving',async()=>{
  agent.adopt('cards',cards(false));
  const before=structuredClone(backend.harness!.fixture.state().snapshot);
  const {container}=render(<App/>);
  await backend.harness!.settle();
  expect(container.querySelectorAll('.scr:not(.scr--group)')).toHaveLength(1);
  expect(screen.getByRole('img',{name:'Accessibility permission missing, please grant it on that device'})).toBeDefined();
  await act(async()=>{fireEvent.click(container.querySelector(`[data-ctx="device:${B}"] .dev__hit`)!);});
  expect(container.querySelector('.scr[data-selected]')).toBeNull();
  expect(backend.harness!.fixture.state().snapshot).toEqual(before);
  await act(async()=>{agent.adopt('cards',{...cards(true),revision:'2'});});
  expect(container.querySelectorAll('.scr:not(.scr--group)')).toHaveLength(3);
  expect(container.querySelector('.dev__accessibility')).toBeNull();
  expect(backend.harness!.of('commit_arrangement')).toHaveLength(0);
  expect(backend.harness!.fixture.state().snapshot).toEqual(before);
});

it('requests local Accessibility and then opens its Settings pane through the existing grant flow',async()=>{
  backend.permissions=[{kind:'accessibility',state:'NotGranted',required:true}];
  agent.adopt('permissions',backend.permissions);
  const before=structuredClone(backend.harness!.fixture.state().snapshot);
  const {container}=render(<App/>);
  await backend.harness!.settle();
  expect(container.querySelectorAll('.scr:not(.scr--group)')).toHaveLength(2);
  const icon=()=>screen.getByRole('button',{name:'Accessibility permission missing, grant access'});
  await act(async()=>{fireEvent.click(icon());});
  expect(backend.harness!.of('request_permission').map(r=>r.params)).toEqual([{kind:'accessibility'}]);
  await act(async()=>{fireEvent.click(icon());});
  expect(backend.harness!.of('open_permission_settings').map(r=>r.params)).toEqual([{kind:'accessibility'}]);
  await act(async()=>{agent.adopt('permissions',[{kind:'accessibility',state:'Granted',required:true}]);});
  expect(container.querySelectorAll('.scr:not(.scr--group)')).toHaveLength(3);
  expect(backend.harness!.of('commit_arrangement')).toHaveLength(0);
  expect(backend.harness!.fixture.state().snapshot).toEqual(before);
});

it('keeps excluded placement and hidden flags intact during another computer edit with auto-compact on',async()=>{
  agent.adopt('cards',cards(false));
  const before=structuredClone(backend.harness!.fixture.state().snapshot!);
  const {container}=render(<App/>);
  await backend.harness!.settle();
  const display=container.querySelector('.scr')!, zoom=cameraStore.get()!.zoom;
  fireEvent.pointerDown(display,{button:0,clientX:600,clientY:300});
  fireEvent.pointerMove(display,{clientX:600+0.9*4096*zoom,clientY:300+0.7*4096*zoom});
  await act(async()=>{fireEvent.pointerUp(display);});
  await backend.harness!.settle();
  const after=backend.harness!.fixture.state().snapshot!;
  expect(backend.harness!.of('commit_arrangement')).toHaveLength(1);
  expect(after.placements.find(p=>p.device===B)).toEqual(before.placements.find(p=>p.device===B));
  expect(after.disabled).toEqual(before.disabled);
  expect(after.native.map(({token:_,...native})=>native)).toEqual(before.native.map(({token:_,...native})=>native));
  expect(after.active_profile_id).toEqual(before.active_profile_id);
});
