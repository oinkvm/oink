// @vitest-environment jsdom
import { act, cleanup, renderHook, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { usePreferences } from './model';
import type { InputTransform } from '../api/types';
const mocks=vi.hoisted(()=>({state:{} as Record<string,unknown>,call:vi.fn(),input:vi.fn(),audio:vi.fn(),clipboard:vi.fn(),login:vi.fn(),group:vi.fn(),transform:vi.fn(),listeners:new Set<()=>void>()}));
vi.mock('../api/store',async()=>{
  const {useSyncExternalStore}=await import('react');
  const subscribe=(listener:()=>void)=>{mocks.listeners.add(listener);return()=>{mocks.listeners.delete(listener);};};
  return {useAgent:()=>useSyncExternalStore(subscribe,()=>mocks.state),agent:{get:()=>mocks.state,onEvent:()=>()=>{},fail:vi.fn(),query:async(key:string,query:()=>Promise<unknown>)=>{const value=await query();mocks.state={...mocks.state,[key]:value};mocks.listeners.forEach(listener=>listener());}}};
});
vi.mock('../api/client',()=>({call:mocks.call,api:{group:mocks.group,setInputTransform:mocks.transform,setInputSettings:mocks.input,setAudioSettings:mocks.audio,setClipboardSettings:mocks.clipboard,setLaunchAtLogin:mocks.login}}));
beforeEach(()=>{
  vi.clearAllMocks();localStorage.clear();
  mocks.group.mockResolvedValue({members:[]});
  mocks.state={localPreferences:{notification_level:'default',ui_language:'system',theme_mode:'system',theme_accent:{light:'#2E7BFF',dark:'#5599FF'},show_status_icon:true,confirm_quit:true,log_level:'info',log_max_mb:1024},
    input:{event_frequency:'unlimited',wake_displays_on_input:true,cursor_trail:false,accept_contacts:true,mac_scroll_feel:true,device_gain:1,emergency_hold_ms:5000,hotkeys:[]},
    clipboard:{text:true,rich_text:true,images:true,files:true,custom:true},audio:{settings:{keep_local_playback:true,receive_output:null,forwarding_enabled:true}}};
  mocks.call.mockResolvedValue(mocks.state.localPreferences);
});
afterEach(cleanup);
it.each(['macos','windows'])('persists a separate custom map through preset changes and a fresh webview on %s',async platform=>{
  const mac=platform==='macos',target=mac?'windows':'macos';
  const keyboard:InputTransform['windows']['keyboard']={preset:'custom',table:[{from:0x700e0,to:0},{from:0x700e1,to:0x700e3},{from:0x700e2,to:0x700e0},{from:0x700e3,to:0x700e6},...(mac?[{from:0xff0003,to:0}]:[])],caps_tap_hold:{tap:0,hold:0}};
  let stored:InputTransform={macos:{reverse_wheel:false,keyboard:{preset:'none',table:[]}},windows:{reverse_wheel:false,keyboard:{preset:'none',table:[]}},overrides:[],[target]:{reverse_wheel:false,keyboard}};
  mocks.state={...mocks.state,boot:{platform},overview:{this_device:{id:'self'}}};
  mocks.group.mockImplementation(async()=>({members:[{is_self:true,input_transform:JSON.parse(JSON.stringify(stored))}]}));
  mocks.transform.mockImplementation(async(_device,value)=>{stored=JSON.parse(JSON.stringify(value));});
  const mounted=renderHook(usePreferences);
  await waitFor(()=>expect(mounted.result.current.canEdit('keymap')).toBe(true));
  const custom=structuredClone(mounted.result.current.prefs.keyCustom[mac?'mac':'win']);
  for(const preset of ['mac','win'] as const){
    await act(async()=>mounted.result.current.set('keyPreset',preset));
    expect(mounted.result.current.prefs.keyCustom[mac?'mac':'win']).toEqual(custom);
    expect(mounted.result.current.prefs.capsSplit).toBe(true);
    expect(mounted.result.current.prefs.allowFnMapping).toBe(mac);
  }
  mounted.unmount();
  localStorage.clear();
  const reloaded=renderHook(usePreferences);
  await waitFor(()=>expect(reloaded.result.current.prefs.keyPreset).toBe('win'));
  expect(reloaded.result.current.prefs.keyCustom[mac?'mac':'win']).toEqual(custom);
  expect(reloaded.result.current.prefs.keyCustomTouched[mac?'mac':'win']).toBe(true);
  await act(async()=>reloaded.result.current.set('keyPreset','custom'));
  expect(stored[target].keyboard.preset).toBe('custom');
  expect(stored[target].keyboard.table).toContainEqual({from:0x700e0,to:0});
  expect(stored[target].keyboard.caps_tap_hold).toEqual({tap:0,hold:0});
  if(mac)expect(stored[target].keyboard.table).toContainEqual({from:0xff0003,to:0});
  expect(localStorage.getItem('oink:prefs')).toBeNull();
});
it('defaults external input forwarding off and persists only its local input patch', async()=>{
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs.forwardInjectedInput).toBe(false);
  expect(result.current.canEdit('forwardInjectedInput')).toBe(true);
  await act(async()=>result.current.set('forwardInjectedInput',true));
  expect(mocks.input).toHaveBeenCalledExactlyOnceWith({forward_injected_input:true});
  expect(localStorage.getItem('oink:prefs')).toBeNull();
});
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
    if(group==='clipboard')result.current.set('clipSend',false);
    else if(group==='audio')result.current.set('keepLocalPlayback',false);
    else result.current.set('pollLimit','125');
  });
  expect(mocks[group]).toHaveBeenCalledWith(group==='clipboard'?{send:false}:group==='audio'?{keep_local_playback:false}:{event_frequency:'hz125'});
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

it('reads the default-on receiver preference and sends only its edited field',async()=>{
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs.macScrollFeel).toBe(true);
  expect(result.current.canEdit('macScrollFeel')).toBe(true);
  await act(async()=>result.current.set('macScrollFeel',false));
  expect(mocks.input).toHaveBeenCalledExactlyOnceWith({mac_scroll_feel:false});
});
it('does not overwrite the child preference when disabling contacts',async()=>{
  const {result}=renderHook(usePreferences);
  await act(async()=>result.current.set('touchpadInject',false));
  expect(mocks.input).toHaveBeenCalledExactlyOnceWith({accept_contacts:false});
  expect(result.current.prefs.macScrollFeel).toBe(true);
});

it.each([['clipReceive','receive'],['clipSend','send'],['removeDsStore','remove_ds_store']] as const)('persists the independent %s clipboard master',async(key,field)=>{
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs[key]).toBe(true);
  await act(async()=>result.current.set(key,false));
  expect(mocks.clipboard).toHaveBeenCalledExactlyOnceWith({[field]:false});
});

it('persists the native gesture child independently of its parent and scroll sibling', async()=>{
  const {result}=renderHook(()=>usePreferences());
  await waitFor(()=>expect(result.current.prefs.nativeMultifinger).toBe(true));
  await act(async()=>result.current.set('nativeMultifinger',false));
  expect(mocks.input).toHaveBeenCalledExactlyOnceWith({native_multifinger:false});
  expect(result.current.prefs.touchpadInject).toBe(true);
  expect(result.current.prefs.macScrollFeel).toBe(true);
});

it('keeps per-device shortcut overrides in the local input settings API',async()=>{
  const h={action:'move_pointer_to',device:'self',keys:['Ctrl','Alt','1'],enabled:true};
  mocks.state={...mocks.state,input:{...(mocks.state.input as object),hotkeys:[h]}};
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs.shortcuts['move:self']).toEqual({on:true,keys:'Control+Alt+1'});
  await act(async()=>result.current.set('shortcuts',{...result.current.prefs.shortcuts,'move:self':{on:false,keys:'Control+Alt+1'}}));
  expect(mocks.input).toHaveBeenCalledExactlyOnceWith({hotkeys:[{...h,enabled:false}]});
  expect(mocks.call).not.toHaveBeenCalled();
  expect(localStorage.getItem('oink:prefs')).toBeNull();
});
it('lets an unassigned OFF device enable its recorder without inventing an empty key',async()=>{
  const h={action:'move_pointer_to',device:'overflow',keys:[],enabled:false};
  mocks.state={...mocks.state,input:{...(mocks.state.input as object),hotkeys:[h]}};
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs.shortcuts['move:overflow']).toEqual({on:false,keys:''});
  await act(async()=>result.current.set('shortcuts',{...result.current.prefs.shortcuts,'move:overflow':{on:true,keys:''}}));
  expect(mocks.input).toHaveBeenCalledExactlyOnceWith({hotkeys:[{...h,enabled:true}]});
});

it('persists the escape hold key as a local single-key shortcut override', async()=>{
  const h={action:'lock_escape',keys:['Meta'],enabled:true};
  mocks.state={...mocks.state,input:{...(mocks.state.input as object),hotkeys:[h]}};
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs.shortcuts.lockEscape).toEqual({on:true,keys:'Meta'});
  await act(async()=>result.current.set('shortcuts',{...result.current.prefs.shortcuts,lockEscape:{on:true,keys:'X'}}));
  expect(mocks.input).toHaveBeenCalledExactlyOnceWith({hotkeys:[{...h,keys:['X']}]});
});

it('enables the default-off shared microphone shortcut through input settings',async()=>{
 const h={action:'toggle_microphone_mute',keys:['Ctrl','Alt','M'],enabled:false};
 mocks.state={...mocks.state,input:{...(mocks.state.input as object),hotkeys:[h]}};
 const {result}=renderHook(usePreferences);
 expect(result.current.prefs.shortcuts.microphoneMute).toEqual({on:false,keys:'Control+Alt+M'});
 await act(async()=>result.current.set('shortcuts',{...result.current.prefs.shortcuts,microphoneMute:{on:true,keys:'Control+Alt+M'}}));
 expect(mocks.input).toHaveBeenCalledExactlyOnceWith({hotkeys:[{...h,enabled:true}]});
});

it('reloads the DS_Store preference from the device clipboard settings without a group edit',async()=>{
  mocks.clipboard.mockImplementation(async patch=>{
    mocks.state={...mocks.state,clipboard:{...(mocks.state.clipboard as object),...patch}};
    mocks.listeners.forEach(listener=>listener());
  });
  const mounted=renderHook(usePreferences);
  expect(mounted.result.current.prefs.removeDsStore).toBe(true);
  await act(async()=>mounted.result.current.set('removeDsStore',false));
  mounted.unmount();
  localStorage.clear();
  const reloaded=renderHook(usePreferences);
  await waitFor(()=>expect(reloaded.result.current.prefs.removeDsStore).toBe(false));
  expect(mocks.clipboard).toHaveBeenCalledExactlyOnceWith({remove_ds_store:false});
  expect(mocks.transform).not.toHaveBeenCalled();
  expect(mocks.call).not.toHaveBeenCalled();
  expect(localStorage.getItem('oink:prefs')).toBeNull();
  reloaded.unmount();
  mocks.state={...mocks.state,clipboard:null};
  const unavailable=renderHook(usePreferences);
  expect(unavailable.result.current.canEdit('removeDsStore')).toBe(false);
});
