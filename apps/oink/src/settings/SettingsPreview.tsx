import { useState } from 'react';
import { OinkProvider, Toaster } from 'oink-ui';
import { setLanguage } from '../i18n';
import { SettingsPage, type PermState } from './SettingsPage';
import { DEFAULT_PREFS, type PermissionId, type Prefs, type PrefsApi } from './prefs';
import type { AutoUnlockFailure } from '../api/client';

// Dev-only preview of the settings page without a backend (?fixture=settings&platform=win&section=tools).
export function SettingsPreview() {
  const q = new URLSearchParams(location.search);
  setLanguage(q.get('lang') ?? 'zh-Hans');
  const platform = q.get('platform') === 'win' ? 'win' : 'mac';
  const theme = q.get('theme') === 'dark' ? 'dark' : 'light';
  const failureParam = q.get('failure');
  const failure = (failureParam === 'password_incorrect' || failureParam === 'windows_hello_only' || failureParam === 'login_ui_not_ready' || failureParam === 'unknown')
    ? failureParam as AutoUnlockFailure : null;
  const autoUnlockFixture = { enabled: failure !== null || q.get('enabled') === '1', failure, windows_hello_only: q.get('hello') === '1' };
  const [prefs, setPrefs] = useState<Prefs>(DEFAULT_PREFS);
  const api = { prefs, set: (k: keyof Prefs, v: never) => setPrefs(p => ({ ...p, [k]: v })), canEdit: () => true } as unknown as PrefsApi;
  const perms: Record<PermissionId, PermState> = { accessibility: 'granted', input: 'granted', network: 'not_required', files: 'granted', audio: 'granted' };
  const section = q.get('section');
  return <OinkProvider defaultTheme={theme} defaultMotion="reduced">
    <div style={{ height: '100vh', position: 'relative' }}>
      <SettingsPage open platform={platform} api={api} capsIme={false} query="" jump={section ? { section: section as never, item: (q.get('item') ?? undefined) as never, n: 1 } : null}
        onActive={() => {}} version="preview" upgrade={{ available: false, next: '' }} perms={perms} onGrant={() => {}} onLog={() => {}} onAbout={() => {}} onBlocklist={() => {}} autoUnlockFixture={autoUnlockFixture}/>
    </div>
    <Toaster/>
  </OinkProvider>;
}
