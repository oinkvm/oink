import { describe, expect, it } from 'vitest';
import { deviceShortcuts, shortcutId } from './Shortcuts';
import { sectionsFor, searchSections } from './schema';
import { setLanguage } from '../i18n';
import type { Hotkey } from '../api/types';
const h = (device: string, enabled = true): Hotkey => ({action:'move_pointer_to',device,keys:enabled?['Ctrl','Alt','1']:[],enabled});
describe('per-device shortcut listing',()=>{
  it('includes self and offline members, follows renames, and removes departed members',()=>{
    setLanguage('en');
    const keys=[h('self'),h('offline',false)];
    const members=[{device:'self',name:'This computer'},{device:'offline',name:'Other computer'}];
    const rows=deviceShortcuts(keys,members);
    expect(rows.map(r=>r.label)).toEqual(['Move pointer to This computer','Move pointer to Other computer']);
    expect(rows.map(r=>r.id)).toEqual(['move:self','move:offline']);
    expect(deviceShortcuts(keys,[members[0],{...members[1],name:'Renamed'}])[1].label).toBe('Move pointer to Renamed');
    expect(deviceShortcuts([keys[0]],members).map(r=>r.id)).toEqual(['move:self']);
    expect(shortcutId(keys[0])).toBe('move:self');
    expect(shortcutId({action:'return_pointer_home',enabled:true,keys:[]})).toBe('recall');
  });
  it('renders device rows in the shortcut settings schema and makes names searchable',()=>{
    setLanguage('en');
    const rows=deviceShortcuts([h('self')],[{device:'self',name:'Test computer'}]);
    expect(sectionsFor('mac',rows).find(s=>s.id==='shortcuts')?.items.map(i=>i.label)).toContain('Move pointer to Test computer');
    expect(searchSections('mac','Test computer',rows).get('shortcuts')).toEqual(new Set(['sc-move:self']));
  });
  it.each([['zh-Hans','将指针移动到 Test'],['ja','ポインタを Test に移動'],['ko','Test(으)로 포인터 이동']])('localizes %s', (lang, label)=>{
    setLanguage(lang);
    expect(deviceShortcuts([h('self')],[{device:'self',name:'Test'}])[0].label).toBe(label);
  });
});
