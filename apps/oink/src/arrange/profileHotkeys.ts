import type { Hotkey } from '../api/types';
import type { ProfileSummary } from '../api/layoutProfiles';
import { SHORTCUTS, shortcutId } from '../settings/Shortcuts';
import { profileName } from './snapshot';
import { tr } from '../i18n';
const canonical = (keys: string[]) => keys.map(k => k === 'Control' ? 'Ctrl' : k).sort().join('+');
export function profileHotkeyConflict(id: string, keys: string, hotkeys: Hotkey[], profiles: Pick<ProfileSummary, 'id' | 'name'>[], members: ReadonlyArray<{device:string;name:string}>): string | null {
  if (!keys) return null;
  const clash = hotkeys.find(h => !(h.action === 'switch_layout' && h.profile === id) && h.keys.length && canonical(h.keys) === canonical(keys.split('+')));
  if (!clash) return null;
  return hotkeyName(clash, profiles, members);
}
export function hotkeyName(clash: Hotkey, profiles: Pick<ProfileSummary, 'id' | 'name'>[], members: ReadonlyArray<{device:string;name:string}>): string {
  if (clash.action === 'switch_layout') {
    const profile = profiles.find(p => p.id === clash.profile);
    return profile ? profileName(profile.name) : clash.profile!;
  }
  if (clash.action === 'move_pointer_to') return tr('将指针移动到 {0}', members.find(m => m.device === clash.device)?.name ?? clash.device!);
  return SHORTCUTS.find(s => s.id === shortcutId(clash))?.label ?? clash.action;
}
