// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { act, cleanup, render, screen, waitFor } from '@testing-library/react';
import { localPackagePath, packageDragAllowed, PackageDropArea, usePackageDropReceiver, usePackageDrag, setPackageDrag } from './PackageDrop';
import { setLanguage } from '../i18n';
const events=vi.hoisted(()=>({native:new Map<string,(event:{payload:unknown})=>void>(),wry:null as null|((event:{payload:unknown})=>void),stops:[] as (()=>void)[]}));
vi.mock('../api/transport',()=>({inTauri:true}));
vi.mock('@tauri-apps/api/event',()=>({listen:vi.fn(async(name:string,callback:(event:{payload:unknown})=>void)=>{events.native.set(name,callback);const stop=vi.fn();events.stops.push(stop);return stop;})}));
vi.mock('@tauri-apps/api/webview',()=>({getCurrentWebview:()=>({onDragDropEvent:async(callback:(event:{payload:unknown})=>void)=>{events.wry=callback;const stop=vi.fn();events.stops.push(stop);return stop;}})}));
beforeEach(()=>{events.native.clear();events.wry=null;events.stops=[];setPackageDrag(false);});
afterEach(()=>{cleanup();setPackageDrag(false);});
function Receiver({receive}:{receive:(path:string)=>void}) {usePackageDropReceiver('mac',receive);return <output>{usePackageDrag()?'drag':'normal'}</output>;}
it('retains the named Enter decision on position-only Over and filters the eventual drop',async()=>{
 const receive=vi.fn();const view=render(<Receiver receive={receive}/>);
 await waitFor(()=>expect(events.wry).not.toBeNull());
 act(()=>events.wry!({payload:{type:'enter',paths:['/Oink.dmg']}}));expect(screen.getByText('drag')).toBeTruthy();
 act(()=>events.wry!({payload:{type:'over',position:{x:1,y:2}}}));expect(screen.getByText('drag')).toBeTruthy();
 act(()=>events.wry!({payload:{type:'drop',paths:['/Oink.dmg']}}));expect(receive).toHaveBeenCalledExactlyOnceWith('/Oink.dmg');expect(screen.getByText('normal')).toBeTruthy();
 act(()=>events.wry!({payload:{type:'enter',paths:['/notes.txt']}}));expect(screen.getByText('normal')).toBeTruthy();
 act(()=>events.wry!({payload:{type:'drop',paths:['/notes.txt']}}));expect(receive).toHaveBeenCalledTimes(1);
 view.unmount();events.stops.forEach(stop=>expect(stop).toHaveBeenCalledOnce());
});
it('receives the native promise/file-URL path and clears hover on exit or drop',async()=>{
 const receive=vi.fn();render(<Receiver receive={receive}/>);
 await waitFor(()=>expect(events.native.has('local-package-drop')).toBe(true));
 act(()=>events.native.get('local-package-drag')!({payload:true}));expect(screen.getByText('drag')).toBeTruthy();
 act(()=>events.native.get('local-package-drop')!({payload:'/received/Oink.dmg'}));expect(screen.getByText('normal')).toBeTruthy();expect(receive).toHaveBeenCalledExactlyOnceWith('/received/Oink.dmg');
 act(()=>events.native.get('local-package-drag')!({payload:true}));
 act(()=>events.wry!({payload:{type:'leave'}}));expect(screen.getByText('normal')).toBeTruthy();
});
it('filters single package names for the receiving platform during drag-over and drop',()=>{
  expect(localPackagePath(['/Oink.DMG'],'mac')).toBe('/Oink.DMG');
  expect(localPackagePath(['C:\\Oink.MSI'],'win')).toBe('C:\\Oink.MSI');
  for(const platform of ['mac','win'] as const) {
    expect(localPackagePath(['/one.dmg','/two.msi'],platform)).toBeNull();
    expect(packageDragAllowed(['/readme.txt'],platform)).toBe(false);
    expect(packageDragAllowed([],platform)).toBe(true);
    expect(packageDragAllowed(null,platform)).toBe(true);
  }
  expect(packageDragAllowed(['/Oink.msi'],'mac')).toBe(false);
  expect(packageDragAllowed(['/Oink.dmg'],'win')).toBe(false);
});
it.each(['zh-Hans','en','ja','ko'] as const)('shows platform-specific copy in %s',language=>{
  setLanguage(language);const view=render(<PackageDropArea platform="mac"/>);
  expect(screen.getByRole('region').textContent).toContain('.dmg');
  view.rerender(<PackageDropArea platform="win"/>);
  expect(screen.getByRole('region').textContent).toContain('.msi');
});
