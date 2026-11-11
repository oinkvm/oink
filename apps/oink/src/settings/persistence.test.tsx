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
  mocks.state={localPreferences:{notification_level:'default',ui_language:'system',theme_mode:'system',theme_accent:{light:'#2E7BFF',dark:'#5599FF'},show_status_icon:true,confirm_quit:true,paste_as_file:true,log_level:'info',log_max_mb:1024},
    input:{event_frequency:'unlimited',wake_displays_on_input:true,cursor_trail:false,accept_contacts:true,touchpad_injection_enabled:true,mac_scroll_feel:true,device_gain:1,emergency_hold_ms:5000,hotkeys:[]},
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
it('makes the keymap editable only in the render that shows the loaded map', async()=>{
  const keyboard:InputTransform['windows']['keyboard']={preset:'custom',table:[{from:0x700e0,to:0x700e3}],caps_tap_hold:null};
  const stored:InputTransform={macos:{reverse_wheel:false,keyboard:{preset:'none',table:[]}},windows:{reverse_wheel:false,keyboard},overrides:[]};
  mocks.state={...mocks.state,boot:{platform:'macos'},overview:{this_device:{id:'self'}}};
  mocks.group.mockResolvedValue({members:[{is_self:true,input_transform:stored}]});
  const renders:{editable:boolean,map:unknown}[]=[];
  renderHook(()=>{const api=usePreferences();renders.push({editable:api.canEdit('keymap'),map:api.prefs.keyCustom.mac});return api;});
  await waitFor(()=>expect(renders.at(-1)?.editable).toBe(true));
  const loaded=renders.at(-1)!.map;
  expect(renders.filter(r=>r.editable).every(r=>JSON.stringify(r.map)===JSON.stringify(loaded))).toBe(true);
  expect(renders[0].map).not.toEqual(loaded);
});
it('keeps the stored touchpad preference while injection is temporarily unavailable', async()=>{
  mocks.state.input={...(mocks.state.input as object),accept_contacts:false,touchpad_injection_enabled:true,contacts_injection_available:false};
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs.touchpadInject).toBe(true);
  await act(async()=>result.current.set('keepAwake',false));
  expect(mocks.input).toHaveBeenCalledExactlyOnceWith({wake_displays_on_input:false});
  expect(result.current.prefs.touchpadInject).toBe(true);
});
it('defaults external input forwarding on and persists only its local input patch', async()=>{
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs.forwardInjectedInput).toBe(true);
  expect(result.current.canEdit('forwardInjectedInput')).toBe(true);
  await act(async()=>result.current.set('forwardInjectedInput',false));
  expect(mocks.input).toHaveBeenCalledExactlyOnceWith({forward_injected_input:false});
  expect(localStorage.getItem('oink:prefs')).toBeNull();
});
it.each([false,true])('preserves explicit external forwarding=%s during unrelated edits', async enabled=>{
  mocks.state.input={...(mocks.state.input as object),forward_injected_input:enabled};
  const mounted=renderHook(usePreferences);
  expect(mounted.result.current.prefs.forwardInjectedInput).toBe(enabled);
  await act(async()=>mounted.result.current.set('keepAwake',false));
  expect(mocks.input).toHaveBeenCalledExactlyOnceWith({wake_displays_on_input:false});
  expect(mounted.result.current.prefs.forwardInjectedInput).toBe(enabled);
  mounted.unmount();
  const reloaded=renderHook(usePreferences);
  expect(reloaded.result.current.prefs.forwardInjectedInput).toBe(enabled);
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
it.each([['125','hz125'],['250','hz250'],['500','hz500']] as const)('maps the %s pointer limit to %s',async(limit,frequency)=>{
  const {result}=renderHook(usePreferences);
  await act(async()=>result.current.set('pollLimit',limit));
  expect(mocks.input).toHaveBeenCalledWith({event_frequency:frequency});
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

it.each([['clipReceive','receive'],['clipSend','send'],['clipDedupe','dedupe'],['removeDsStore','remove_ds_store']] as const)('persists the independent %s clipboard master',async(key,field)=>{
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

it('defaults Quicker compatibility off and saves only that local field',async()=>{
  mocks.input.mockImplementation(async patch=>{
    mocks.state={...mocks.state,input:{...(mocks.state.input as object),...patch}};
    mocks.listeners.forEach(listener=>listener());
  });
  const mounted=renderHook(usePreferences);
  expect(mounted.result.current.prefs.quickerCompat).toBe(false);
  expect(mounted.result.current.canEdit('quickerCompat')).toBe(true);
  mounted.unmount();
  for(const enabled of [true,false]) {
    const editing=renderHook(usePreferences);
    mocks.input.mockClear();
    await act(async()=>editing.result.current.set('quickerCompat',enabled));
    expect(mocks.input).toHaveBeenCalledExactlyOnceWith({quicker_compat:enabled});
    editing.unmount();
    const reloaded=renderHook(usePreferences);
    expect(reloaded.result.current.prefs.quickerCompat).toBe(enabled);
    reloaded.unmount();
  }
});

it('defaults foreground capture off for old settings and saves only the local edited field',async()=>{
  mocks.input.mockImplementation(async patch=>{
    mocks.state={...mocks.state,input:{...(mocks.state.input as object),...patch}};
    mocks.listeners.forEach(listener=>listener());
  });
  const mounted=renderHook(usePreferences);
  expect(mounted.result.current.prefs.takeForegroundOnLeave).toBe(false);
  expect(mounted.result.current.canEdit('takeForegroundOnLeave')).toBe(true);
  mounted.unmount();
  for(const enabled of [true,false]) {
    const editing=renderHook(usePreferences);
    mocks.input.mockClear();
    await act(async()=>editing.result.current.set('takeForegroundOnLeave',enabled));
    expect(mocks.input).toHaveBeenCalledExactlyOnceWith({take_foreground_on_leave:enabled});
    editing.unmount();
    const reloaded=renderHook(usePreferences);
    expect(reloaded.result.current.prefs.takeForegroundOnLeave).toBe(enabled);
    expect(reloaded.result.current.prefs.forwardInjectedInput).toBe(true);
    reloaded.unmount();
  }
  expect(mocks.transform).not.toHaveBeenCalled();
  expect(mocks.call).not.toHaveBeenCalled();
  expect(localStorage.getItem('oink:prefs')).toBeNull();
});

it('defaults sync unlock off and saves only that local input field',async()=>{
  mocks.input.mockImplementation(async patch=>{
    mocks.state={...mocks.state,input:{...(mocks.state.input as object),...patch}};
    mocks.listeners.forEach(listener=>listener());
  });
  const mounted=renderHook(usePreferences);
  expect(mounted.result.current.prefs.syncUnlock).toBe(false);
  expect(mounted.result.current.canEdit('syncUnlock')).toBe(true);
  mounted.unmount();
  for(const enabled of [true,false]) {
    const editing=renderHook(usePreferences);
    mocks.input.mockClear();
    await act(async()=>editing.result.current.set('syncUnlock',enabled));
    expect(mocks.input).toHaveBeenCalledExactlyOnceWith({sync_unlock_others:enabled});
    editing.unmount();
    const reloaded=renderHook(usePreferences);
    expect(reloaded.result.current.prefs.syncUnlock).toBe(enabled);
    reloaded.unmount();
  }
});

it.each(['macos','windows'])('defaults source wheel lines on and persists only its local patch on %s', async platform => {
  mocks.state={...mocks.state,boot:{platform}};
  const {result,unmount}=renderHook(usePreferences);
  expect(result.current.prefs.forwardWheelByLines).toBe(true);
  expect(result.current.canEdit('forwardWheelByLines')).toBe(true);
  await act(async()=>result.current.set('forwardWheelByLines',false));
  expect(mocks.input).toHaveBeenCalledExactlyOnceWith({forward_wheel_by_lines:false});
  expect(mocks.transform).not.toHaveBeenCalled();
  expect(localStorage.getItem('oink:prefs')).toBeNull();
  unmount();
  mocks.state={...mocks.state,input:{...(mocks.state.input as object),forward_wheel_by_lines:false}};
  const reloaded=renderHook(usePreferences);
  expect(reloaded.result.current.prefs.forwardWheelByLines).toBe(false);
});

it('persists source encoding quality independently from the diagnostic chart', async()=>{
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs.audioEncodingQuality).toBe('quality');
  await act(async()=>result.current.set('audioEncodingQuality','lossless'));
  expect(mocks.audio).toHaveBeenCalledWith({quality:'lossless'});
});


it('persists enabling and rebinding the open-window shortcut', async () => {
  const h = {action:'open_window',enabled:false,keys:['Ctrl','Alt','A']};
  mocks.state={...mocks.state,input:{...(mocks.state.input as object),hotkeys:[h]}};
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs.shortcuts.openWindow).toEqual({on:false,keys:'Control+Alt+A'});
  await act(async()=>result.current.set('shortcuts',{...result.current.prefs.shortcuts,openWindow:{on:true,keys:'Control+Shift+W'}}));
  await waitFor(()=>expect(mocks.input).toHaveBeenCalledExactlyOnceWith({hotkeys:[{...h,enabled:true,keys:['Ctrl','Shift','W']}]}));
});

it.each(['smooth','quality','lossless'])('keeps explicit source encoding quality %s', quality=>{
  mocks.state={...mocks.state,audio:{settings:{quality,keep_local_playback:true,receive_output:null}}};
  const {result}=renderHook(usePreferences);
  expect(result.current.prefs.audioEncodingQuality).toBe(quality);
  expect(mocks.audio).not.toHaveBeenCalled();
});

it('preserves the actual setting rejection instead of reporting backend failure',async()=>{
  const {agent}=await import('../api/store');
  const reason={code:'invalid_argument',message:'两个快捷键不能相同',problem:{code:'invalid_value',unknownCode:null,nativeCode:null,retryAfterMs:null}};
  mocks.input.mockRejectedValueOnce(reason);
  const {result}=renderHook(usePreferences);
  await act(async()=>result.current.set('pollLimit','125'));
  expect(agent.fail).toHaveBeenCalledWith('invalid_value','两个快捷键不能相同');
  expect(result.current.prefs.pollLimit).toBe('none');
});

it('saves Paste as File only through local preferences and restores its value',async()=>{
  const mounted=renderHook(usePreferences);
  expect(mounted.result.current.prefs.pasteAsFile).toBe(true);
  await act(async()=>mounted.result.current.set('pasteAsFile',false));
  expect(mocks.call).toHaveBeenCalledExactlyOnceWith('set_ui_preferences',{settings:{paste_as_file:false}});
  expect(mocks.clipboard).not.toHaveBeenCalled();
  expect(mocks.input).not.toHaveBeenCalled();
  expect(localStorage.getItem('oink:prefs')).toBeNull();
  mounted.unmount();
  mocks.state={...mocks.state,localPreferences:{...(mocks.state.localPreferences as object),paste_as_file:false}};
  const restored=renderHook(usePreferences);
  expect(restored.result.current.prefs.pasteAsFile).toBe(false);
});
