// @vitest-environment jsdom
import { act, cleanup, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { Arrangement } from './Arrangement';
import { cameraStore } from './cameraStore';
import { useState } from 'react';
import type { Screen } from './geometry';
let time=0,id=0;
const frames=new Map<number,FrameRequestCallback>();
const tick=()=>act(()=>{time+=1000/60;const all=[...frames.values()];frames.clear();all.forEach(f=>f(time));});
beforeEach(()=>{
  vi.useFakeTimers();time=0;id=0;frames.clear();
  vi.stubGlobal('ResizeObserver',class {observe(){} disconnect(){}});
  vi.stubGlobal('PointerEvent',MouseEvent);
  vi.stubGlobal('requestAnimationFrame',(f:FrameRequestCallback)=>{frames.set(++id,f);return id;});
  vi.stubGlobal('cancelAnimationFrame',(n:number)=>frames.delete(n));
  vi.spyOn(performance,'now').mockImplementation(()=>time);
  vi.spyOn(HTMLElement.prototype,'getBoundingClientRect').mockImplementation(()=>({left:0,top:0,x:0,y:0,width:900,height:600,right:900,bottom:600,toJSON(){}}));
  HTMLElement.prototype.setPointerCapture=()=>{};
});
afterEach(()=>{cleanup();vi.restoreAllMocks();vi.unstubAllGlobals();vi.useRealTimers();});
it('a resize press and release without motion does not compact or save layout',()=>{
  const screens:Screen[]=[{id:'a',device:'local',name:'Local',x:0,y:0,w:700,h:500,wallpaper:0},{id:'b',device:'remote',name:'Remote',x:1200,y:0,w:700,h:500,wallpaper:0}];
  const preview=vi.fn(),save=vi.fn();
  const {container}=render(<Arrangement screens={screens} onProjection={preview} onScreensChange={save} selectedId="a" onSelect={()=>{}} settings={{showHotZones:false,snapping:false,compact:true,padding:48,step:10}}/>);
  for(let n=0;n<100&&frames.size;n++)tick();
  const screen=container.querySelector('.scr')!;
  const ox=parseFloat((screen as HTMLElement).style.left),oy=parseFloat((screen as HTMLElement).style.top);
  const zoom=cameraStore.get()!.zoom;
  fireEvent.pointerDown(container.querySelector('.scr [data-c="se"]')!,{button:0,clientX:ox+700*zoom,clientY:oy+500*zoom});
  expect(preview.mock.lastCall?.[0]).not.toBeNull();
  fireEvent.pointerUp(screen);
  expect(preview.mock.lastCall?.[0]).not.toBeNull();
  expect(save).not.toHaveBeenCalled();
});
it('selects an unselected screen on press and projects without saving layout',()=>{
  const screens:Screen[]=[{id:'a',device:'local',name:'Local',x:0,y:0,w:700,h:500,wallpaper:0}];
  const preview=vi.fn(),save=vi.fn();
  function Canvas(){const [selected,setSelected]=useState<string|null>(null);return <Arrangement screens={screens} onProjection={preview} onScreensChange={save} selectedId={selected} onSelect={setSelected} settings={{showHotZones:false,snapping:false,compact:false,padding:48,step:10}}/>;}
  const {container}=render(<Canvas/>);
  for(let n=0;n<100&&frames.size;n++)tick();
  const screen=container.querySelector('.scr')!;
  fireEvent.pointerDown(screen,{button:0,clientX:200,clientY:200});
  expect(preview.mock.lastCall?.[0]).toHaveLength(1);
  fireEvent.pointerUp(screen);
  expect(preview.mock.lastCall?.[0]).toHaveLength(1);
  expect(save).not.toHaveBeenCalled();
});
it.each([false,true])('shows on press without movement, including a same-batch click (%s)',quick=>{
  const screens:Screen[]=[{id:'a',device:'local',name:'Local',x:0,y:0,w:700,h:500,wallpaper:0},{id:'b',device:'remote',name:'Remote',x:700,y:0,w:700,h:500,wallpaper:0}];
  const preview=vi.fn(),save=vi.fn();
  const {container}=render(<Arrangement screens={screens} onProjection={preview} onScreensChange={save} selectedId="a" onSelect={()=>{}} settings={{showHotZones:false,snapping:false,compact:false,padding:48,step:10}}/>);
  for(let n=0;n<100&&frames.size;n++)tick();
  preview.mockClear();
  const screen=container.querySelector('.scr')!;
  if(quick)act(()=>{fireEvent.pointerDown(screen,{button:0,clientX:200,clientY:200});fireEvent.pointerUp(screen);});
  else {
    fireEvent.pointerDown(screen,{button:0,clientX:200,clientY:200});
    expect(preview.mock.lastCall?.[0]).not.toBeNull();
    expect(preview).toHaveBeenCalled();
    act(()=>vi.advanceTimersByTime(3000));tick();
    expect(preview.mock.lastCall?.[0]).not.toBeNull();
    fireEvent.pointerUp(screen);
  }
  expect(preview).toHaveBeenCalled();
  expect(preview.mock.calls.every(([value])=>value!==null)).toBe(true);
  act(()=>vi.advanceTimersByTime(1199));expect(preview.mock.lastCall?.[0]).not.toBeNull();
  act(()=>vi.advanceTimersByTime(1));expect(preview.mock.lastCall?.[0]).toBeNull();
  expect(save).not.toHaveBeenCalled();
});
it.each(['move','resize'] as const)('projects rigid group %s and every spring-back frame, then closes after lingering',mode=>{
  const screens:Screen[]=[
    {id:'a',device:'local',name:'Local',x:0,y:0,w:700,h:500,wallpaper:0},
    {id:'b',device:'local',name:'Local',x:700,y:100,w:350,h:300,wallpaper:0},
    {id:'c',device:'remote',name:'Remote',x:1200,y:0,w:700,h:500,wallpaper:0},
  ];
  const preview=vi.fn();
  const {container,unmount}=render(<Arrangement screens={screens} onProjection={preview} onScreensChange={()=>{}}
    selectedId="a" onSelect={()=>{}} settings={{showHotZones:false,snapping:false,compact:false,padding:48,step:10}}/>);
  for(let n=0;n<100&&frames.size;n++)tick();
  expect(preview.mock.lastCall?.[0]).toBeNull();
  const screen=container.querySelector('.scr')!;
  const target=mode==='move'?screen:container.querySelector('.scr--group [data-c="se"]')!;
  const box=mode==='move'?screens[0]:{x:0,y:0,w:1050,h:500};
  const camera=cameraStore.get()!;
  const zoom=camera.zoom;
  // The rendered screen gives the current camera translation.
  const ox=parseFloat((screen as HTMLElement).style.left),oy=parseFloat((screen as HTMLElement).style.top);
  const x=ox+(mode==='move'?200:box.w)*zoom,y=oy+(mode==='move'?200:box.h)*zoom;
  fireEvent.pointerDown(target,{button:0,clientX:x,clientY:y});
  fireEvent.pointerMove(screen,{clientX:x+40,clientY:y+20});
  tick();
  const live=preview.mock.lastCall?.[0] as Screen[];
  expect(live).not.toBeNull();expect(live[0]).not.toEqual(screens[0]);
  const k=live[0].w/screens[0].w;
  expect(live[1].x-live[0].x).toBeCloseTo(700*k,8);
  expect(live[1].y-live[0].y).toBeCloseTo(100*k,8);
  expect(live[1].w/live[0].w).toBeCloseTo(0.5,8);
  fireEvent.pointerCancel(screen);
  tick();
  const rebound=preview.mock.lastCall?.[0] as Screen[];
  expect(rebound[0]).not.toEqual(live[0]);
  for(let n=0;n<200&&frames.size;n++)tick();
  expect(preview.mock.lastCall?.[0]).not.toBeNull();
  act(()=>vi.advanceTimersByTime(1200));
  expect(preview.mock.lastCall?.[0]).toBeNull();
  unmount();expect(preview.mock.lastCall?.[0]).toBeNull();
});
it('starts a native screen preview on press before any pointer motion',()=>{
  const screens:Screen[]=[{id:'a',device:'local',name:'Local',x:0,y:0,w:700,h:500,wallpaper:0},{id:'b',device:'local',name:'Local',x:700,y:0,w:700,h:500,wallpaper:0}];
  const move=vi.fn();
  const {container}=render(<Arrangement screens={screens} onScreensChange={()=>{}} selectedId="a" onSelect={()=>{}}
    screenEditable={()=>true} onScreenMove={move}
    settings={{showHotZones:false,snapping:false,compact:false,padding:48,step:10}}/>);
  for(let n=0;n<100&&frames.size;n++)tick();
  fireEvent.pointerDown(container.querySelector('.scr')!,{button:0,clientX:200,clientY:200});
  expect(move).toHaveBeenCalledWith({type:'drag',id:'a',desired:{x:0,y:0}});
});
