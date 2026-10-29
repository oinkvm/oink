import { describe, expect, it } from 'vitest';
import { editTransform, readKeyboardMapping } from './model';
import { DEFAULT_PREFS } from './prefs';
import type { InputTransform } from '../api/types';
describe('target-specific key mapping edits',()=>{
  it('retains hidden target OS values, full-table entries and per-device overrides',()=>{
    const current:InputTransform={macos:{reverse_wheel:true,keyboard:{preset:'custom',table:[{from:10,to:20}]}},windows:{reverse_wheel:false,keyboard:{preset:'custom',table:[{from:40,to:41},{from:0x700e0,to:0x700e3}]}},overrides:[{target:'target',reverse_wheel:true,keyboard:null}]};
    const result=editTransform(current,'mac',{...DEFAULT_PREFS,scrollInvert:true,keyPreset:'custom'});
    expect(result.macos).toBe(current.macos);expect(result.overrides).toBe(current.overrides);
    expect(result.windows.reverse_wheel).toBe(false);expect(result.windows.keyboard.table).toContainEqual({from:40,to:41});
  });
  it('uses an exclusive Caps tap/hold mapping without a duplicate ordinary Caps entry',()=>{
    const current:InputTransform={macos:{reverse_wheel:false,keyboard:{preset:'none',table:[]}},windows:{reverse_wheel:false,keyboard:{preset:'none',table:[]}},overrides:[]};
    const prefs={...DEFAULT_PREFS,keyPreset:'custom' as const,capsSplit:true,keyCustom:{...DEFAULT_PREFS.keyCustom,mac:{keys:{caps:'shift',control:'win',option:'alt',command:'ctrl',shift:'shift'},capsLong:'caps'}}};
    const keyboard=editTransform(current,'mac',prefs).windows.keyboard;
    expect(keyboard.caps_tap_hold).toEqual({tap:0x700e1,hold:0x70039});expect(keyboard.table.some(m=>m.from===0x70039)).toBe(false);
  });
});

describe('source mouse wheel direction',()=>{
  it.each(['mac','win'] as const)('uses one local switch for every target from %s',platform=>{
    const keyboard={preset:'none' as const,table:[]};
    const current:InputTransform={macos:{reverse_wheel:false,keyboard},windows:{reverse_wheel:false,keyboard},overrides:[{target:'same-os',reverse_wheel:false,keyboard},{target:'other-os',reverse_wheel:true,keyboard:null}]};
    for(const invert of [true,false]){
      const result=editTransform(current,platform,{...DEFAULT_PREFS,scrollInvert:invert},invert);
      expect(result.macos.reverse_wheel).toBe(invert);
      expect(result.windows.reverse_wheel).toBe(invert);
      expect(result.overrides.every(o=>o.reverse_wheel===null)).toBe(true);
      expect(result.overrides[0].keyboard).toBe(keyboard);
      expect(result.overrides).toHaveLength(1);
    }
  });
});

describe('None and Fn mapping persistence',()=>{
  it('stores None as the empty HID and removes the Fn entry when disabled, preserving overrides',()=>{
    const keyboard={preset:'custom' as const,table:[{from:0xff0003,to:0x700e0}]};
    const current:InputTransform={macos:{reverse_wheel:false,keyboard},windows:{reverse_wheel:false,keyboard},overrides:[{target:'peer',reverse_wheel:null,keyboard}]};
    const prefs={...DEFAULT_PREFS,keyPreset:'custom' as const,allowFnMapping:true,keyCustom:{...DEFAULT_PREFS.keyCustom,mac:{keys:{control:'none',fn:'rightAlt'}}}};
    const result=editTransform(current,'mac',prefs);
    expect(result.windows.keyboard.table).toContainEqual({from:0x700e0,to:0});
    expect(result.windows.keyboard.table).toContainEqual({from:0xff0003,to:0x700e6});
    expect(result.overrides).toBe(current.overrides);
    const off=editTransform(result,'mac',{...prefs,allowFnMapping:false});
    expect(off.windows.keyboard.table.some(m=>m.from===0xff0003)).toBe(false);
  });
});

describe('persisted custom mapping projection',()=>{
  it.each(['ctrl','alt','shift','win','rightCtrl','rightAlt','rightShift','rightWin','none'])('roundtrips Fn mapped to %s',to=>{
    const keyboard={preset:'custom' as const,table:[]};
    const current:InputTransform={macos:{reverse_wheel:false,keyboard},windows:{reverse_wheel:false,keyboard},overrides:[]};
    const prefs={...DEFAULT_PREFS,keyPreset:'custom' as const,allowFnMapping:true,keyCustom:{...DEFAULT_PREFS.keyCustom,mac:{keys:{fn:to,caps:'none'},capsLong:'none'}},capsSplit:true};
    const result=editTransform(current,'mac',prefs);
    const decoded=readKeyboardMapping(result.windows.keyboard,'mac');
    expect(decoded.allowFnMapping).toBe(true);
    expect(decoded.map.keys.fn).toBe(to);
    expect(decoded.map.keys.caps).toBe('none');
    expect(decoded.map.capsLong).toBe('none');
    const off=readKeyboardMapping(editTransform(result,'mac',{...prefs,allowFnMapping:false}).windows.keyboard,'mac');
    expect(off.allowFnMapping).toBe(false);
    expect(off.map.keys.fn).toBe(to);
  });
});

describe('custom mapping independent of active preset',()=>{
  it('uses an older writer\'s active Custom edit while retaining only disabled assignments from its backup',()=>{
    const saved_custom={table:[{from:0x700e0,to:0},{from:0x700e4,to:0},{from:0xff0003,to:0x700e6}],caps_hold:0,caps_split:true,allow_fn:true};
    const keyboard={preset:'custom' as const,table:[{from:0x700e0,to:0x700e7}],caps_tap_hold:null,saved_custom};
    const current:InputTransform={macos:{reverse_wheel:false,keyboard:{preset:'none',table:[]}},windows:{reverse_wheel:false,keyboard},overrides:[]};
    const decoded=readKeyboardMapping(keyboard,'mac');
    expect(decoded.map.keys.control).toBe('rightWin');
    expect(decoded.allowFnMapping).toBe(false);
    expect(decoded.capsSplit).toBe(false);
    expect(decoded.map.keys.fn).toBe('rightAlt');
    expect(decoded.map.capsLong).toBe('none');
    const prefs={...DEFAULT_PREFS,keyPreset:'custom' as const,keyCustomTouched:{mac:true,win:false},keyCustom:{...DEFAULT_PREFS.keyCustom,mac:decoded.map},capsSplit:decoded.capsSplit,allowFnMapping:decoded.allowFnMapping};
    const edited=editTransform(current,'mac',prefs);
    expect(edited.windows.keyboard.saved_custom?.table).not.toContainEqual({from:0x700e4,to:0});
  });
  it('keeps hidden custom entries inactive under presets and restores them with their owner overrides',()=>{
    const hidden={from:0x700e4,to:0};
    const keyboard={preset:'custom' as const,table:[hidden]};
    const override={target:'peer',reverse_wheel:null,keyboard};
    const current:InputTransform={macos:{reverse_wheel:true,keyboard},windows:{reverse_wheel:false,keyboard},overrides:[override]};
    const prefs={...DEFAULT_PREFS,keyPreset:'custom' as const,keyCustomTouched:{mac:true,win:false}};
    const custom=editTransform(current,'mac',prefs);
    const preset=editTransform(custom,'mac',{...prefs,keyPreset:'mac'});
    expect(preset.windows.keyboard.table).not.toContainEqual(hidden);
    expect(preset.windows.keyboard.saved_custom?.table).toContainEqual(hidden);
    const restored=editTransform(preset,'mac',prefs);
    expect(restored.windows.keyboard.table).toContainEqual(hidden);
    expect(restored.macos).toBe(current.macos);
    expect(restored.overrides).toBe(current.overrides);
  });
  it.each(['mac','win'] as const)('restores custom keys after both presets and a serialized reload on %s',platform=>{
    const keyboard={preset:'none' as const,table:[]};
    let stored:InputTransform={macos:{reverse_wheel:false,keyboard},windows:{reverse_wheel:false,keyboard},overrides:[]};
    const map=platform==='mac'?{keys:{control:'none',option:'ctrl',command:'rightAlt',shift:'win',caps:'none',fn:'rightWin'},capsLong:'none'}:{keys:{ctrl:'none',win:'option',alt:'shift',shift:'command',caps:'none'}};
    const custom={...DEFAULT_PREFS,keyPreset:'custom' as const,keyCustomTouched:{mac:true,win:true},allowFnMapping:platform==='mac',capsSplit:platform==='mac',keyCustom:{...DEFAULT_PREFS.keyCustom,[platform]:map}};
    stored=editTransform(stored,platform,custom);
    const target=platform==='mac'?'windows':'macos';
    const expected=readKeyboardMapping(stored[target].keyboard,platform);
    for(const keyPreset of ['mac','win'] as const){
      stored=JSON.parse(JSON.stringify(editTransform(stored,platform,{...custom,keyPreset})));
      const decoded=readKeyboardMapping(stored[target].keyboard,platform);
      expect(decoded.keyPreset).toBe(keyPreset);
      expect(decoded.map).toEqual(expected.map);
      expect(decoded.allowFnMapping).toBe(expected.allowFnMapping);
      expect(stored[target].keyboard.caps_tap_hold).toBeNull();
      const restored=editTransform(stored,platform,{...custom,keyPreset:'custom'});
      expect(restored[target].keyboard.table).toEqual(editTransform(restored,platform,custom)[target].keyboard.table);
    }
  });
  it('retains disabled Fn and Caps assignments for the next enable after a preset reload',()=>{
    const keyboard={preset:'none' as const,table:[]};
    const stored:InputTransform={macos:{reverse_wheel:false,keyboard},windows:{reverse_wheel:false,keyboard},overrides:[]};
    const prefs={...DEFAULT_PREFS,keyPreset:'custom' as const,keyCustomTouched:{mac:true,win:false},allowFnMapping:false,capsSplit:false,keyCustom:{...DEFAULT_PREFS.keyCustom,mac:{keys:{control:'none',fn:'rightAlt',caps:'shift'},capsLong:'none'}}};
    const active=editTransform(stored,'mac',prefs);
    const preset=JSON.parse(JSON.stringify(editTransform(active,'mac',{...prefs,keyPreset:'mac'})));
    const decoded=readKeyboardMapping(preset.windows.keyboard,'mac');
    expect(decoded.map.keys.fn).toBe('rightAlt');
    expect(decoded.map.capsLong).toBe('none');
    expect(decoded.allowFnMapping).toBe(false);
    expect(decoded.capsSplit).toBe(false);
  });
});
