// @vitest-environment jsdom
import { act, cleanup, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { usePreferences } from './model';
const mocks=vi.hoisted(()=>({state:{} as Record<string,unknown>,call:vi.fn(),input:vi.fn(),audio:vi.fn(),clipboard:vi.fn(),login:vi.fn(),listeners:new Set<()=>void>()}));
vi.mock('../api/store',async()=>{
  const {useSyncExternalStore}=await import('react');
  const subscribe=(listener:()=>void)=>{mocks.listeners.add(listener);return()=>{mocks.listeners.delete(listener);};};
  return {useAgent:()=>useSyncExternalStore(subscribe,()=>mocks.state),agent:{get:()=>mocks.state,onEvent:()=>()=>{},fail:vi.fn(),query:async(key:string,query:()=>Promise<unknown>)=>{const value=await query();mocks.state={...mocks.state,[key]:value};mocks.listeners.forEach(listener=>listener());}}};
});
vi.mock('../api/client',()=>({call:mocks.call,api:{group:async()=>({members:[]}),setInputSettings:mocks.input,setAudioSettings:mocks.audio,setClipboardSettings:mocks.clipboard,setLaunchAtLogin:mocks.login}}));
beforeEach(()=>{
  vi.clearAllMocks();localStorage.clear();
  mocks.state={localPreferences:{notification_level:'default',ui_language:'system',theme_mode:'system',theme_accent:{light:'#2E7BFF',dark:'#5599FF'},show_status_icon:true,confirm_quit:true,log_level:'info',log_max_mb:1024},
    input:{event_frequency:'unlimited',wake_displays_on_input:true,cursor_trail:false,accept_contacts:true,device_gain:1,emergency_hold_ms:5000,hotkeys:[]},
    clipboard:{text:true,rich_text:true,images:true,files:true,custom:true},audio:{settings:{keep_local_playback:true,receive_output:null,forwarding_enabled:true}}};
  mocks.call.mockResolvedValue(mocks.state.localPreferences);
});
afterEach(cleanup);
it.each([false,true])('Settings reflects the onboarding-confirmed login=%s and can edit it afterwards',async enabled=>{
  mocks.state.launch={state:'disabled',problem:null,configured:false};
  mocks.login.mockImplementation(async on=>({state:on?'enabled':'disabled',problem:null,configured:true}));
  const {result,rerender}=renderHook(usePreferences);
  expect(mocks.login).not.toHaveBeenCalled();
  await act(async()=>result.current.confirmLaunchAtLogin(enabled));
  await act(async()=>{rerender();});
  expect(mocks.login).toHaveBeenCalledExactlyOnceWith(enabled);
  expect(mocks.state.launch).toEqual({state:enabled?'enabled':'disabled',problem:null,configured:true});
  expect(result.current.prefs.launchAtLogin).toBe(enabled);
  await act(async()=>result.current.set('launchAtLogin',!enabled));
  await act(async()=>{rerender();});
  expect(result.current.prefs.launchAtLogin).toBe(!enabled);
});
it('sends only the changed UI preference',async()=>{
  const {result}=renderHook(usePreferences);
  await act(async()=>result.current.set('confirmQuit',false));
  expect(mocks.call).toHaveBeenCalledWith('set_ui_preferences',{settings:{confirm_quit:false}});
});
it('passes a local appearance patch without copying effective defaults',async()=>{
  const {result}=renderHook(usePreferences);
  await act(async()=>result.current.saveLocal({theme_mode:'dark'}));
  expect(mocks.call).toHaveBeenCalledWith('set_ui_preferences',{settings:{theme_mode:'dark'}});
});
it.each(['clipboard','audio','input'] as const)('sends only the changed %s preference',async group=>{
  const {result}=renderHook(usePreferences);
  await act(async()=>{
    if(group==='clipboard')result.current.set('clipFiles',false);
    else if(group==='audio')result.current.set('keepLocalPlayback',false);
    else result.current.set('pollLimit','125');
  });
  expect(mocks[group]).toHaveBeenCalledWith(group==='clipboard'?{files:false}:group==='audio'?{keep_local_playback:false}:{event_frequency:'hz125'});
});
it('persists only edited presentation fields',()=>{
  const {result}=renderHook(usePreferences);
  act(()=>result.current.savePresentation({motion:'reduced'}));
  expect(JSON.parse(localStorage.getItem('oink:prefs')!)).toEqual({motion:'reduced'});
});
it('defaults sponsor visibility without persisting it, then retains only explicit overrides',()=>{
  const {result,unmount}=renderHook(usePreferences);
  expect(result.current.presentation.hide_sponsor).toBe(false);
  expect(localStorage.getItem('oink:prefs')).toBeNull();
  act(()=>result.current.savePresentation({hide_sponsor:true}));
  expect(JSON.parse(localStorage.getItem('oink:prefs')!)).toEqual({hide_sponsor:true});
  act(()=>result.current.savePresentation({motion:'reduced'}));
  expect(JSON.parse(localStorage.getItem('oink:prefs')!)).toEqual({hide_sponsor:true,motion:'reduced'});
  unmount();
  const reloaded=renderHook(usePreferences);
  expect(reloaded.result.current.presentation.hide_sponsor).toBe(true);
  expect(mocks.call).not.toHaveBeenCalled();
});

it.each([false,true])('does not rewrite explicit forwarding=%s when loading or editing audio preferences',async enabled=>{
  (mocks.state.audio as {settings:{forwarding_enabled:boolean}}).settings.forwarding_enabled=enabled;
  const {result}=renderHook(usePreferences);
  expect(mocks.audio).not.toHaveBeenCalled();
  await act(async()=>result.current.set('keepLocalPlayback',false));
  expect(mocks.audio).toHaveBeenCalledExactlyOnceWith({keep_local_playback:false});
  expect((mocks.state.audio as {settings:{forwarding_enabled:boolean}}).settings.forwarding_enabled).toBe(enabled);
});

it.each(['disabled','minimal','default','verbose'] as const)('persists notification level %s as one device-local field',async level=>{
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs.notificationLevel).toBe('default');
  expect(result.current.canEdit('notificationLevel')).toBe(true);
  if(level==='default')return;
  await act(async()=>result.current.set('notificationLevel',level));
  expect(mocks.call).toHaveBeenCalledWith('set_ui_preferences',{settings:{notification_level:level}});
  expect(mocks.input).not.toHaveBeenCalled();
  expect(localStorage.getItem('oink:prefs')).toBeNull();
});
