import { useState } from 'react';
import type { LayoutProfilesState, ProfileSummary } from '../api/layoutProfiles';
import { setLanguage, tr } from '../i18n';
import { ProfileList } from './LayoutProfiles';
import { profileName } from './snapshot';

// Dev-only preview of Settings › Layout › Saved layouts with per-profile shortcuts.
const profile = (id: string, name: ProfileSummary['name'], active: boolean): ProfileSummary =>
  ({ id, name, content_revision: id, last_used_ms: '0', exact_match: active, active });
const state = {
  epoch: 'preview', sequence: '1', token: 'preview', catalog_token: '1', readiness: 'ready',
  profiles: [profile('a', { kind: 'default' }, true), profile('b', { kind: 'custom', value: 'demo' }, false), profile('c', { kind: 'custom', value: '会议室' }, false)],
  active_profile_id: 'a', create_enabled: true, snapshot: null, devices: [], pending: [], operation: null,
} as LayoutProfilesState;

export function ProfileHotkeyPreview() {
  setLanguage(new URLSearchParams(location.search).get('lang') ?? 'zh-Hans');
  const platform = new URLSearchParams(location.search).get('platform') === 'win' ? 'win' : 'mac';
  const [hotkeys, setHotkeys] = useState<Record<string, string>>({ b: 'Control+Alt+Shift+2' });
  return <div style={{ padding: 24, maxWidth: 640, background: 'var(--ok-raised)', minHeight: '100vh', boxSizing: 'border-box' }}>
    <ProfileList state={state} platform={platform} hotkeys={hotkeys}
      onHotkey={(p, keys) => {
        const clash = keys && Object.entries(hotkeys).find(([id, k]) => id !== p.id && k === keys);
        if (clash) return tr('和「{0}」的快捷键重复', profileName(state.profiles.find(q => q.id === clash[0])!.name));
        setHotkeys(h => { const { [p.id]: _, ...rest } = h; return keys ? { ...rest, [p.id]: keys } : rest; });
        return null;
      }}
      onRename={async () => null} onDelete={async () => {}}/>
  </div>;
}
