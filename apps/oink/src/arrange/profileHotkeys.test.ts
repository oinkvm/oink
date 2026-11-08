import { describe, expect, it } from 'vitest';
import { tr } from '../i18n';
import { profileHotkeyConflict } from './profileHotkeys';
import type { Hotkey } from '../api/types';
describe('layout shortcut conflict', () => {
  const global: Hotkey = {action:'open_window',keys:['Alt','Ctrl','A'],enabled:false};
  const profile: Hotkey = {action:'switch_layout',profile:'work',keys:['Ctrl','Shift','F9'],enabled:true};
  const names = [{id:'work',name:{kind:'custom' as const,value:'Office'}}];
  it('compares canonical modifiers and disabled global assignments', () => {
    expect(profileHotkeyConflict('home','Control+Alt+A',[global],[],[])).toBe(tr('打开 Oink 窗口'));
  });
  it('compares device targets and other layouts but excludes itself and clears', () => {
    const move: Hotkey = {action:'move_pointer_to',device:'peer',keys:['Ctrl','Alt','1'],enabled:true};
    expect(profileHotkeyConflict('home','Alt+Control+1',[move],[],[{device:'peer',name:'Laptop'}])).toBe(tr('将指针移动到 {0}', 'Laptop'));
    expect(profileHotkeyConflict('home','Shift+Control+F9',[profile],names,[])).toBe('Office');
    expect(profileHotkeyConflict('work','Control+Shift+F9',[profile],names,[])).toBeNull();
    expect(profileHotkeyConflict('home','',[global],[],[])).toBeNull();
  });
});
