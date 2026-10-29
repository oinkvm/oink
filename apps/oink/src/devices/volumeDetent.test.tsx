// @vitest-environment jsdom
import { cleanup, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeAll, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { VolumeRow } from './VolumeRow';
import { pointerVolume } from './volumeDetent';
import type { Device } from './types';
const native=vi.hoisted(()=>({invoke:vi.fn().mockResolvedValue(undefined)}));
vi.mock('@tauri-apps/api/core',()=>({invoke:native.invoke,isTauri:()=>true}));
const device:Device={id:'peer',name:'Peer',os:'mac',group:'self',ip:'',port:0,latency:null,lastSeen:null,cursor:'local',screen:'shared',mouse:'shared',keyboard:'shared',clipboard:'shared',gameMode:'off',
  speaker:{mode:'forwarding',volume:99,muted:false},
  microphone:{mode:'source',volume:99,muted:false,permissionGranted:true,virtualReady:true,canSetVolume:true}};
beforeAll(()=>{
  vi.stubGlobal('PointerEvent',MouseEvent);
  HTMLElement.prototype.setPointerCapture=()=>{};
  Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})});
});
afterEach(()=>{cleanup();native.invoke.mockClear();});

it.each([[0,0],[96,99],[97,100],[100,100],[103,100],[104,101],[200,200]])('maps raw %s to %s', (raw,value)=>{
  expect(pointerVolume(raw,200)).toBe(value);
});
it('maps both outer ranges linearly and preserves monotonicity and endpoints',()=>{
  for(let raw=0;raw<=200;raw++){
    expect(pointerVolume(raw,200)).toBeGreaterThanOrEqual(pointerVolume(raw-1,200));
    if(raw<=96)expect(pointerVolume(raw,200)).toBe(Math.round(raw*99/96));
    if(raw>=104)expect(pointerVolume(raw,200)).toBe(Math.round(101+(raw-104)*99/96));
  }
  for(let raw=0;raw<=100;raw++)expect(pointerVolume(raw,100)).toBe(raw);
});
function setup(kind:'speaker'|'microphone'|'exit'){
  const change=vi.fn();
  const d=kind==='exit'?{...device,speaker:{...device.speaker,mode:'exit' as const}}:device;
  const {container}=render(<OinkProvider><VolumeRow d={d} microphone={kind==='microphone'} open tabbable onSpeaker={change}/></OinkProvider>);
  const slider=container.querySelector<HTMLElement>('[role="slider"]')!;
  slider.getBoundingClientRect=()=>({left:0,top:0,x:0,y:0,width:kind==='exit'?100:200,height:20,right:200,bottom:20,toJSON(){}});
  return {slider,change};
}
it.each(['speaker','microphone'] as const)('%s pointer haptic fires once per entry, not per move inside the band',kind=>{
  const {slider,change}=setup(kind);
  fireEvent.pointerDown(slider,{button:0,clientX:96,pointerId:1});
  expect(native.invoke).not.toHaveBeenCalled();
  for(const clientX of [97,98,100,103,101,97])fireEvent.pointerMove(slider,{clientX,pointerId:1});
  expect(native.invoke).toHaveBeenCalledExactlyOnceWith('shell_alignment_haptic');
  expect(slider.getAttribute('aria-valuenow')).toBe('100');
  expect(slider.style.getPropertyValue('--v')).toBe('0.5');
  expect(slider.querySelector('.ok-meter__value')?.textContent).toBe('100%');
  fireEvent.pointerMove(slider,{clientX:104,pointerId:1});
  fireEvent.pointerMove(slider,{clientX:103,pointerId:1});
  expect(native.invoke).toHaveBeenCalledTimes(2); // even a rapid, distinct re-entry
  fireEvent.pointerUp(slider,{pointerId:1});
  expect(change.mock.calls.at(-1)![0]).toMatchObject({value:100,final:true});
  fireEvent.pointerDown(slider,{button:0,clientX:100,pointerId:2});
  fireEvent.pointerUp(slider,{pointerId:2});
  expect(native.invoke).toHaveBeenCalledTimes(3); // a track click uses the same mapping
});
it.each(['speaker','microphone'] as const)('%s keyboard and wheel pass through 100 exactly, without a haptic',kind=>{
  const {slider,change}=setup(kind);
  fireEvent.keyDown(slider,{key:'ArrowRight'});
  fireEvent.keyDown(slider,{key:'ArrowRight'});
  fireEvent.keyDown(slider,{key:'ArrowLeft'});
  fireEvent.wheel(slider,{deltaY:1});
  expect(change.mock.calls.map(([a])=>a.value)).toEqual([100,101,100,99]);
  expect(native.invoke).not.toHaveBeenCalled();
});
it('does not change pointer values or fire haptics on a 0–100 output bar',()=>{
  const {slider,change}=setup('exit');
  for(const clientX of [96,97,100]){
    fireEvent.pointerDown(slider,{button:0,clientX,pointerId:1});
    fireEvent.pointerUp(slider,{pointerId:1});
  }
  expect(change.mock.calls.map(([a])=>a.value)).toEqual([96,96,97,97,100,100]);
  expect(native.invoke).not.toHaveBeenCalled();
});
