// @vitest-environment jsdom
import { cleanup, renderHook } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { AgentState } from '../api/store';
const {read}=vi.hoisted(()=>({read:vi.fn()}));
vi.mock('../api/store',()=>({useAgent:read,agent:{onEvent:()=>()=>{},fail:vi.fn()}}));
import { usePairing } from './usePairing';
import { setLanguage } from '../i18n';
const row=(device:string)=>({device,name:'Same name',os:'macos',group:{group:'stale advertised group',root:null},count:1,addresses:[],selected:{family:0,address:[192,0,2,1],port:47470,interface_scope:0},source:0,identity:'advertised' as const,reachable:true,compatible:true,problem:null});
afterEach(cleanup);
describe('nearby membership projection shared by the add-device and menu-bar panels',()=>{
  it.each([true,false])('reacts to member add/remove by DeviceId (main window: %s)',activeWindow=>{
    setLanguage('en');
    let members:string[]=[];
    read.mockImplementation(()=>({boot:null,nearby:{revision:'1',rows:[row('member'),row('stranger')]},cards:{cards:members.map(device=>({device}))},blocked:null,pairings:null,pairingEvents:[]} as unknown as AgentState));
    const {result,rerender}=renderHook(()=>usePairing(activeWindow));
    expect(result.current.nearby.map(p=>p.id)).toEqual(['member','stranger']);
    members=['member'];rerender();
    expect(result.current.nearby.map(p=>p.id)).toEqual(['stranger']);
    members=[];rerender();
    expect(result.current.nearby.map(p=>p.id)).toEqual(['member','stranger']);
  });
});
