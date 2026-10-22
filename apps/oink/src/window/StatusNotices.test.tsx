// @vitest-environment jsdom
import { StrictMode } from 'react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { act, cleanup, render, screen, waitFor } from '@testing-library/react';
import { OinkProvider, Toaster, toast } from 'oink-ui';
import { agent } from '../api/store';
import type { Alert, Overview, PermissionState } from '../api/types';
import { setLanguage } from '../i18n';
import { StatusNotices } from './StatusNotices';

beforeAll(()=>{
  Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})});
  vi.stubGlobal('ResizeObserver',class {observe(){} unobserve(){} disconnect(){}});
});
afterEach(async()=>{
  act(()=>{agent.reset();toast.dismiss();});
  await waitFor(()=>expect(document.querySelector('.ok-toast')).toBeNull());
  cleanup();document.body.innerHTML='';
});
const permissionAlert=(state:PermissionState='NotGranted'):Alert=>({
  id:'permissions-missing',level:'error',at_ms:0,
  kind:{type:'permission_missing',permissions:[
    {kind:'accessibility',state,required:true},
    {kind:'input_monitoring',state,required:true},
  ]},problem:{code:state==='Denied'?'permission_denied':'permission_required',unknownCode:null,nativeCode:null,retryAfterMs:null},
});
const overview=(alerts:Alert[]):Overview=>({
  this_device:{id:'0'.repeat(64),name:'This device',os:'macos',fingerprint:'',os_version:'',model:null,app_version:''},
  group:null,service:'degraded',sharing_enabled:true,
  pause:{paused:false,scope:'local',revision:0,author:null,author_name:null,emergency:false,reason:"none",capture_detached:true,settling:false},
  focus_lock:null,focus:null,onboarding_done:false,alerts,
});
function show(language:'en'|'zh-Hans'){
  setLanguage(language);
  const surface=document.createElement('div');document.body.append(surface);
  render(<StrictMode><OinkProvider locale={language==='en'?'en-US':'zh-CN'} defaultMotion="reduced"><StatusNotices/><Toaster container={surface} position="bottom-right" mobilePosition="bottom-right" max={3}/></OinkProvider></StrictMode>,{container:surface});
  return surface;
}
describe('status notices',()=>{
  it('shows one completed display failure per device and keeps the receipt readable after recovery',async()=>{
    const failure:Alert={id:'display-info:device',at_ms:0,level:'warning',kind:{type:'display_enumeration_failed',device:'2'.repeat(64)},problem:null};
    show('en');
    expect(screen.queryByText('Display information unavailable')).toBeNull();
    act(()=>agent.receive({type:'overview',data:overview([failure])}));
    expect(await screen.findAllByText('Display information unavailable')).toHaveLength(1);
    act(()=>agent.receive({type:'overview',data:overview([{...failure,at_ms:1}])}));
    expect(screen.getAllByText('Display information unavailable')).toHaveLength(1);
    act(()=>agent.receive({type:'overview',data:overview([])}));
    expect(screen.getAllByText('Display information unavailable')).toHaveLength(1);
  });
  it('shows a completed capture failure even while onboarding is unfinished',async()=>{
    const failure:Alert={id:'capture-unavailable',at_ms:0,level:'error',kind:{type:'capture_unavailable'},problem:{code:'capture_unavailable',unknownCode:null,nativeCode:null,retryAfterMs:null}};
    const status=overview([failure]);
    expect(status.onboarding_done).toBe(false);
    agent.receive({type:'overview',data:status});
    show('en');
    expect(await screen.findAllByText('Input capture unavailable')).toHaveLength(2);
    expect(document.querySelectorAll('.ok-toast')).toHaveLength(1);
    // Recovery does not dismiss a real failed-attempt receipt before it can be read.
    const dismiss=vi.spyOn(toast,'dismiss');
    act(()=>agent.receive({type:'overview',data:overview([])}));
    expect(dismiss).not.toHaveBeenCalledWith('status:capture-unavailable');
    expect(screen.getAllByText('Input capture unavailable')).toHaveLength(2);
    dismiss.mockRestore();
  });
  it('retains the backlog reason until sharing resumes',async()=>{
    const paused=overview([]);
    paused.pause={...paused.pause,paused:true,reason:'input_backlog'};
    agent.receive({type:'overview',data:paused});
    show('en');
    expect(await screen.findAllByText('Input backlog exceeded 5 seconds; sharing paused')).toHaveLength(1);
    act(()=>agent.receive({type:'overview',data:paused}));
    expect(document.querySelectorAll('.ok-toast')).toHaveLength(1);
    act(()=>agent.receive({type:'overview',data:overview([])}));
    await waitFor(()=>expect(screen.queryByText('Input backlog exceeded 5 seconds; sharing paused')).toBeNull());
  });
  it.each(['en','zh-Hans'] as const)('never shows permission notices in %s, including changed snapshots',async language=>{
    await agent.query('overview',async()=>overview([permissionAlert()]));
    const surface=show(language);
    for(const state of ['NotDetermined','NotGranted','Denied','Granted'] as const) {
      act(()=>agent.receive({type:'overview',data:overview([permissionAlert(state)])}));
      expect(surface.querySelector('.ok-toast')).toBeNull();
      expect(surface.textContent).toBe('');
    }
  });
  it('still reconciles other alerts by ID alongside hidden permission alerts',async()=>{
    const alert:Alert={id:'offline',at_ms:0,level:'warning',kind:{type:'peer_offline',device:'2'.repeat(64)},problem:null};
    agent.receive({type:'overview',data:overview([permissionAlert(),alert])});
    show('en');
    expect(await screen.findAllByText('Device is offline')).toHaveLength(1);
    for(let i=1;i<=3;i++) act(()=>agent.receive({type:'overview',data:overview([permissionAlert('Denied'),{...alert,at_ms:i}])}));
    expect(screen.getAllByText('Device is offline')).toHaveLength(1);
    expect(document.querySelectorAll('.ok-toast')).toHaveLength(1);
    act(()=>agent.receive({type:'overview',data:overview([permissionAlert()])}));
    await waitFor(()=>expect(screen.queryByText('Device is offline')).toBeNull());
  });
});
