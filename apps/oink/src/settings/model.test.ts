import { describe, expect, it } from 'vitest';
import { editTransform } from './model';
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
