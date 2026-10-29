import { useEffect, useRef, useState } from 'react';
import { api, call } from '../api/client';
import { agent, useAgent } from '../api/store';
import type { InputTransform, KeyboardMapping } from '../api/types';
import { setLanguage } from '../i18n';
import { shortcutId } from './Shortcuts';
import { DEFAULT_PREFS, presetMap, type Prefs, type PrefsApi, type KeyMap } from './prefs';

export type LocalPreferences = import('../api/cards').UiPreferences;
/** Device-local UI preferences kept by this webview only (not backend or group data). */
export interface PresentationPreferences { motion: 'auto' | 'reduced' | 'full'; hide_sponsor: boolean; compact_layout: boolean }
export interface ReportStatus { configured:boolean;delivery_disabled:boolean;consent: {choice:'unasked'|'denied'|'granted';policy_version:number;generation:number} }
const hid = (usage:number) => (7 << 16) | usage;
const usage: Record<string,number> = {control:hid(224),ctrl:hid(224),shift:hid(225),option:hid(226),alt:hid(226),command:hid(227),win:hid(227),caps:hid(57),fn:0xff0003,none:0,rightCtrl:hid(228),rightShift:hid(229),rightAlt:hid(230),rightWin:hid(231)};
const keyCode = (key:string) => usage[key] ?? Number(key.replace(/^hid:/,''));
function storePresentation(current: PresentationPreferences, patch: Partial<PresentationPreferences>): PresentationPreferences {
  let stored: Partial<PresentationPreferences> = {};
  try { stored = JSON.parse(localStorage.getItem('oink:prefs') ?? '{}'); } catch { /* Start with no explicit overrides. */ }
  const next = {...stored,...patch};
  try { localStorage.setItem('oink:prefs', JSON.stringify(next)); }
  catch { return {...current,...patch}; } // Unavailable storage keeps the edit for this session only.
  return readPresentation();
}
function readPresentation(): PresentationPreferences {
  try { const stored = JSON.parse(localStorage.getItem('oink:prefs') ?? '{}'); return { motion: ['auto','reduced','full'].includes(stored.motion) ? stored.motion : 'auto', hide_sponsor: stored.hide_sponsor === true, compact_layout: stored.compact_layout !== false }; }
  catch { return {motion:'auto',hide_sponsor:false,compact_layout:true}; }
}
export function editTransform(current: InputTransform, platform: 'mac'|'win', prefs: Prefs, reverseWheel?: boolean): InputTransform {
  const target = platform === 'mac' ? 'windows' : 'macos';
  const previous=current[target].keyboard, map=prefs.keyCustom[platform];
  const entries=Object.entries(map.keys);
  if(platform==='mac'&&prefs.allowFnMapping&&!entries.some(([from])=>from==='fn'))entries.push(['fn','ctrl']);
  const edited=new Set(entries.map(([from])=>usage[from]));
  // The saved table owns all custom entries, including hidden keys and disabled
  // Fn/Caps assignments. Preset tables are active outputs, never this backup.
  const priorCustom=previous.preset==='custom'?previous.table:previous.saved_custom?.table??[];
  const rememberedFn=platform==='mac'&&previous.preset==='custom'&&!priorCustom.some(m=>m.from===usage.fn)?previous.saved_custom?.table.filter(m=>m.from===usage.fn)??[]:[];
  const customTable=[...[...priorCustom,...rememberedFn].filter(m=>!edited.has(m.from)),...entries.map(([from,to])=>({from:usage[from],to:keyCode(to)}))];
  const saved=prefs.keyPreset==='custom'||prefs.keyCustomTouched[platform]?{
    table:customTable,caps_hold:map.capsLong===undefined?null:keyCode(map.capsLong),caps_split:prefs.capsSplit,allow_fn:platform==='mac'&&prefs.allowFnMapping,
  }:previous.saved_custom;
  const presetEntries=prefs.keyPreset==='custom'?[]:Object.entries(presetMap(platform,prefs.keyPreset,false).keys).map(([from,to])=>({from:usage[from],to:keyCode(to)}));
  const table=prefs.keyPreset==='custom'
    ?customTable.filter(m=>platform!=='mac'||prefs.allowFnMapping||m.from!==usage.fn)
    :presetEntries.flatMap(m=>m.from>=hid(224)&&m.from<=hid(227)&&m.to>=hid(224)&&m.to<=hid(227)?[m,{from:m.from+4,to:m.to+4}]:[m]);
  const keyboard:KeyboardMapping={preset:prefs.keyPreset==='custom'?'custom':prefs.keyPreset==='mac'?'mac_feel':'win_feel',table,saved_custom:saved,
    caps_tap_hold:prefs.keyPreset==='custom'&&prefs.capsSplit&&map.capsLong!==undefined?{tap:keyCode(map.keys.caps??'caps'),hold:keyCode(map.capsLong)}:null};
  if(keyboard.caps_tap_hold)keyboard.table=table.filter(m=>m.from!==usage.caps);
  const result = {...current,[target]:{...current[target],keyboard}};
  // Wheel direction belongs to this source mouse, regardless of its target.
  // Keyboard tables and per-device keyboard overrides remain independent.
  if (reverseWheel !== undefined) {
    result.macos = {...result.macos,reverse_wheel:reverseWheel};
    result.windows = {...result.windows,reverse_wheel:reverseWheel};
    result.overrides = result.overrides.filter(o=>o.keyboard!==null).map(o=>({...o,reverse_wheel:null}));
  }
  return result;
}
/** Decode the source-owned table without inventing a disabled Fn identity mapping. */
export function readKeyboardMapping(keyboard: KeyboardMapping, platform: 'mac'|'win') {
  const destinationNames=platform==='mac'?['ctrl','win','alt','shift','caps','none','rightCtrl','rightShift','rightAlt','rightWin']:['control','option','command','shift','caps','none'];
  const saved=keyboard.saved_custom,activeCustom=keyboard.preset==='custom';
  // Old writers preserve unknown metadata but can still edit the active table.
  // In Custom, that table wins; the backup only supplies disabled assignments.
  const table=activeCustom?keyboard.table:saved?.table??keyboard.table;
  const rememberedFn=saved?.table.find(m=>m.from===usage.fn);
  const allowFnMapping=platform==='mac'&&(activeCustom?keyboard.table.some(m=>m.from===usage.fn):saved?.allow_fn??false);
  const sourceNames=platform==='mac'?['control','option','command','shift','caps',...(table.some(m=>m.from===usage.fn)||rememberedFn?['fn']:[])]:['ctrl','win','alt','shift','caps'];
  const name=(code:number)=>destinationNames.find(n=>usage[n]===code)??`hid:${code}`;
  const caps=activeCustom||!saved?keyboard.caps_tap_hold:null,hold=caps?.hold??saved?.caps_hold;
  const map:KeyMap={keys:Object.fromEntries(sourceNames.map(n=>[n,name(caps&&n==='caps'?caps.tap:table.find(m=>m.from===usage[n])?.to??(n==='fn'?rememberedFn?.to:undefined)??usage[n])])),capsLong:hold==null?undefined:name(hold)};
  const keyPreset=keyboard.preset==='mac_feel'?'mac' as const:keyboard.preset==='win_feel'?'win' as const:'custom' as const;
  return {map,keyPreset,allowFnMapping,capsSplit:activeCustom?!!keyboard.caps_tap_hold:saved?.caps_split??!!keyboard.caps_tap_hold,hasCustom:!!saved||activeCustom};
}
export function usePreferences(): PrefsApi {
  const state = useAgent();
  const [prefs,setPrefs] = useState(DEFAULT_PREFS);
  const [presentation,setPresentation] = useState(readPresentation);
  const [transform,setTransform] = useState<InputTransform|null>(null);
  const report=state.report;
  const [pending,setPending] = useState(false);
  const busy = useRef(false), queued = useRef<Prefs|null>(null), committed = useRef(prefs), latest = useRef(prefs);
  latest.current = prefs;
  const local = state.localPreferences;
  const platform = state.boot?.platform === 'windows' ? 'win' : 'mac';
  useEffect(()=>{
    void api.group().then(group=>setTransform(group.members.find(m=>m.is_self)?.input_transform??null)).catch(()=>{});
    const refresh = () => {void Promise.allSettled([agent.query('launch',api.launchAtLoginStatus),agent.query('fileProvider',api.fileProviderStatus)]);};
    window.addEventListener('focus',refresh); return () => window.removeEventListener('focus',refresh);
  },[]);
  useEffect(()=>agent.onEvent(event=>{if(event.type==='group')setTransform(event.data.members.find(m=>m.is_self)?.input_transform??null);else if(event.type==='input_transform_edited')void api.group().then(group=>setTransform(group.members.find(m=>m.is_self)?.input_transform??null)).catch(()=>{});}),[]);
  useEffect(()=>{
    if(local) setLanguage(local.ui_language);
    if(busy.current)return;
    setPrefs(p=>{
      const next:Prefs={...p,
        ...(local?{notificationLevel:local.notification_level,showInMenuBar:local.show_status_icon,confirmQuit:local.confirm_quit,language:local.ui_language,logLevel:local.log_level,logMaxMb:String(local.log_max_mb)}:{}),
        ...(state.input?{pollLimit:state.input.event_frequency==='hz125'?'125':'none',keepAwake:state.input.wake_displays_on_input,pointerTrail:state.input.cursor_trail,touchpadInject:state.input.accept_contacts,nativeMultifinger:state.input.native_multifinger??true,macScrollFeel:state.input.mac_scroll_feel,forwardInjectedInput:state.input.forward_injected_input??false,pointerGain:state.input.device_gain,escStop:state.input.emergency_hold_ms!==0,
          shortcuts:{...Object.fromEntries(Object.entries(p.shortcuts).filter(([id])=>!id.startsWith('move:'))),...Object.fromEntries(state.input.hotkeys.filter(h=>shortcutId(h)).map(h=>[shortcutId(h),{on:h.enabled,keys:h.keys.map(k=>k==='Ctrl'?'Control':k).join('+')}]))}} as const:{}),
        ...(state.clipboard?{clipReceive:state.clipboard.receive ?? true,clipSend:state.clipboard.send ?? true,removeDsStore:state.clipboard.remove_ds_store ?? true}:{}),
        ...(state.audio?{keepLocalPlayback:state.audio.settings.keep_local_playback,audioDevice:state.audio.settings.receive_output??'default'}:{}),
        ...(state.launch?{launchAtLogin:state.launch.state==='enabled'||state.launch.state==='requires_approval'}:{}),
        ...(state.diagnostics?{diagnostics:state.diagnostics.configured_enabled}:{}),
        ...(report?{crashReports:report.consent.choice==='granted'}:{}),
      }; committed.current=next; return next;
    });
  },[local,state.input,state.clipboard,state.audio,state.launch,state.diagnostics,report,presentation,pending]);
  useEffect(()=>{
    if(!transform||busy.current)return;
    const target=platform==='mac'?'windows':'macos',keyboard=transform[target].keyboard;
    const decoded=readKeyboardMapping(keyboard,platform);
    setPrefs(p=>{const next={...p,keyPreset:decoded.keyPreset,scrollInvert:transform[platform==='mac'?'macos':'windows'].reverse_wheel,capsSplit:decoded.capsSplit,allowFnMapping:decoded.allowFnMapping,
      keyCustom:{...p.keyCustom,[platform]:decoded.map},keyCustomTouched:{...p.keyCustomTouched,[platform]:decoded.hasCustom}};
      committed.current=next;return next;});
  },[transform,platform,pending]);
  const save = async(next:Prefs):Promise<void>=>{
    setPrefs(next);
    if(busy.current){queued.current=next;return;}
    busy.current=true;setPending(true);
    const previous=committed.current;
    const changed=(key:keyof Prefs)=>JSON.stringify(next[key])!==JSON.stringify(previous[key]);
    try{
      const current=agent.get(), appliedLocal=current.localPreferences;
      if(changed('launchAtLogin')){await agent.query('launch',()=>api.setLaunchAtLogin(next.launchAtLogin));next={...next,launchAtLogin:['enabled','requires_approval'].includes(agent.get().launch?.state??'')};}
      if(changed('diagnostics'))await api.setDiagnosticsEnabled(next.diagnostics);
      if((['pollLimit','keepAwake','pointerTrail','touchpadInject','nativeMultifinger','macScrollFeel','forwardInjectedInput','escStop','pointerGain','shortcuts'] as const).some(changed)){
        if(!current.input)throw new Error('unavailable');
        await api.setInputSettings({
          ...(changed('pollLimit')?{event_frequency:next.pollLimit==='125'?'hz125':'unlimited'}:{}),
          ...(changed('keepAwake')?{wake_displays_on_input:next.keepAwake}:{}),
          ...(changed('pointerTrail')?{cursor_trail:next.pointerTrail}:{}),
          ...(changed('touchpadInject')?{accept_contacts:next.touchpadInject}:{}),
          ...(changed('nativeMultifinger')?{native_multifinger:next.nativeMultifinger}:{}),
          ...(changed('macScrollFeel')?{mac_scroll_feel:next.macScrollFeel}:{}),
          ...(changed('forwardInjectedInput')?{forward_injected_input:next.forwardInjectedInput}:{}),
          ...(changed('escStop')?{emergency_hold_ms:next.escStop?5000:0}:{}),
          ...(changed('pointerGain')?{device_gain:next.pointerGain}:{}),
          ...(changed('shortcuts')?{hotkeys:current.input.hotkeys.map(h=>{const key=shortcutId(h),shortcut=key?next.shortcuts[key]:null;return shortcut?{...h,enabled:shortcut.on,keys:shortcut.keys.split('+').filter(Boolean).map(k=>k==='Control'?'Ctrl':k)}:h;})}:{}),
        });
      }
      if((['clipReceive','clipSend','removeDsStore'] as const).some(changed)){
        if(!current.clipboard)throw new Error('unavailable');
        await api.setClipboardSettings({...(changed('clipReceive')?{receive:next.clipReceive}:{}),...(changed('clipSend')?{send:next.clipSend}:{}),...(changed('removeDsStore')?{remove_ds_store:next.removeDsStore}:{})});
      }
      if(changed('keepLocalPlayback')||changed('audioDevice')){
        if(!current.audio)throw new Error('unavailable');
        await api.setAudioSettings({...(changed('keepLocalPlayback')?{keep_local_playback:next.keepLocalPlayback}:{}),...(changed('audioDevice')?{receive_output:next.audioDevice==='default'?null:next.audioDevice}:{})});
      }
      if((['showInMenuBar','confirmQuit','notificationLevel','language','logLevel','logMaxMb'] as const).some(changed)){
        if(!appliedLocal)throw new Error('unavailable');
        await agent.query('localPreferences',()=>call<LocalPreferences>('set_ui_preferences',{settings:{...(changed('notificationLevel')?{notification_level:next.notificationLevel}:{}),...(changed('showInMenuBar')?{show_status_icon:next.showInMenuBar}:{}),...(changed('confirmQuit')?{confirm_quit:next.confirmQuit}:{}),...(changed('language')?{ui_language:next.language}:{}),...(changed('logLevel')?{log_level:next.logLevel}:{}),...(changed('logMaxMb')?{log_max_mb:Number(next.logMaxMb)}:{})}}));
      }
      if((['scrollInvert','keyPreset','keyCustom','capsSplit','allowFnMapping'] as const).some(changed)){
        if(!transform||!current.overview)throw new Error('unavailable');
        const edited=editTransform(transform,platform,next,changed('scrollInvert')?next.scrollInvert:undefined);await api.setInputTransform(current.overview.this_device.id,edited);setTransform(edited);
      }
      if(changed('crashReports')){
        if(!report)throw new Error('unavailable');
        await call<null>('set_report_consent',{expected_generation:report.consent.generation,choice:next.crashReports?'granted':'denied',policy_version:report.consent.policy_version});
        await agent.query('report',()=>call<ReportStatus>('report_status'));
      }
      committed.current=next;setPrefs(queued.current??next);
    }catch{setPrefs(previous);queued.current=null;agent.fail('unavailable');}
    finally{busy.current=false;setPending(false);const follow=queued.current;queued.current=null;if(follow)void save(follow);}
  };
  return{prefs,pending,launchStatus:state.launch,canEdit: key => {
      if(['language','showInMenuBar','confirmQuit','notificationLevel','logLevel','logMaxMb','theme','accent'].includes(key))return local!==null;
      if(['keymap','scrollInvert'].includes(key))return transform!==null;
      if(['pollLimit','pointerGain','keepAwake','pointerTrail','touchpadInject','nativeMultifinger','macScrollFeel','forwardInjectedInput','escStop'].includes(key)||key.startsWith('sc-'))return state.input!==null;
      if(key.startsWith('clip')||key==='removeDsStore')return state.clipboard!==null;
      if(['audioDevice','keepLocalPlayback'].includes(key))return state.audio!==null;
      if(key==='launchAtLogin')return state.launch!==null;
      if(key==='diagnostics')return state.diagnostics!==null;
      if(key==='crashReports')return report!==null;
      return true;
    },chooseReport: async allow => { if(!report) throw new Error('unavailable'); await call<null>('set_report_consent',{expected_generation:report.consent.generation,choice:allow?'granted':'denied',policy_version:report.consent.policy_version}); await agent.query('report',()=>call<ReportStatus>('report_status')); },ready:local!==null,local,presentation,report,hasOverrides:(transform?.overrides.length??0)>0,
    confirmLaunchAtLogin:async enabled=>{await agent.query('launch',()=>api.setLaunchAtLogin(enabled));},
    saveLocal:async patch=>{const current=agent.get().localPreferences;if(!current)throw new Error('unavailable');await agent.query('localPreferences',()=>call<LocalPreferences>('set_ui_preferences',{settings:patch}));},
    savePresentation:patch=>{setPresentation(storePresentation(presentation,patch));},
    set:(key,value)=>{void save({...latest.current,[key]:value});},setPrefs:update=>{void save(update(latest.current));},
  };
}
