// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { setLanguage } from '../i18n';
import { agent } from '../api/store';
import { api } from '../api/client';
import type { LaunchAtLoginStatus } from '../api/types';
import type { PermissionId, PrefsApi } from '../settings/prefs';
import type { PermState } from '../settings/SettingsPage';
import { OnboardingPermissions } from './OnboardingPermissions';

vi.mock('oink-ui', async importOriginal => ({ ...await importOriginal<typeof import('oink-ui')>(), speed: () => 0 }));

vi.mock('../api/store', () => ({ agent: { fail: vi.fn() } }));
beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: true, addEventListener() {}, removeEventListener() {} }) });
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
});
beforeEach(() => { setLanguage('en'); vi.clearAllMocks(); });
afterEach(() => { cleanup(); vi.useRealTimers(); });
const disabled: LaunchAtLoginStatus = { state: 'disabled', problem: null, configured: false };
const missing = { accessibility: 'not_granted', input: 'not_granted', network: 'not_required', files: 'unsupported', audio: 'not_required' } as const;
const granted = { ...missing, accessibility: 'granted', input: 'granted' } as const;
function fixture(status: LaunchAtLoginStatus|null = disabled) {
  const preferences = { launchStatus: status, confirmLaunchAtLogin: vi.fn().mockResolvedValue(undefined) } as unknown as PrefsApi;
  const props = { preferences, perms: missing as Record<PermissionId, PermState>, onGrant: vi.fn(), onDone: vi.fn(), container: null, leaving: false };
  const view = () => <OinkProvider defaultMotion="reduced"><OnboardingPermissions {...props}/></OinkProvider>;
  return { props, view, save: vi.mocked(preferences.confirmLaunchAtLogin) };
}
async function skip() {
  fireEvent.click(screen.getByRole('button', { name: 'Skip' }));
  fireEvent.click(await screen.findByRole('button', { name: 'Confirm' }));
}

it.each([
  ['zh-Hans', 'Oink 需要依靠系统权限来实现共享功能，缺少权限会影响部分功能可用性。'],
  ['en', 'Oink needs system permissions for sharing. Some features are unavailable without them.'],
  ['ja', 'Oink の共有機能にはシステムの権限が必要です。権限がないと一部の機能を利用できません。'],
  ['ko', 'Oink의 공유 기능에는 시스템 권한이 필요하며, 권한이 없으면 일부 기능을 사용할 수 없습니다.'],
])('keeps only the permission summary in the %s subtitle', (lang, subtitle) => {
  setLanguage(lang);
  const {view}=fixture(); render(view());
  expect(document.querySelector('.ob-perms .ob-sub')?.textContent).toBe(subtitle);
});

it('puts the macOS switch first in the shared permission list and applies only on confirmed Skip', async () => {
  const { view, props, save } = fixture(); render(view());
  const toggle = screen.getByRole('switch', {name:'Launch at login'}) as HTMLInputElement;
  expect(toggle.checked).toBe(true);
  expect(document.querySelectorAll('.ob-perms__card')).toHaveLength(1);
  const rows = document.querySelectorAll('.ob-perms__card > .permission-row');
  expect([...rows].map(row => row.querySelector('.ok-row__label')?.textContent)).toEqual(['Launch at login', 'Accessibility', 'Input Monitoring', 'File Provider']);
  expect(rows[0].contains(toggle)).toBe(true);
  expect(rows[0].querySelector('.ok-row__desc')?.textContent).toBe('If turned off, you will need to open Oink manually after restarting to resume sharing.');
  fireEvent.click(toggle);
  expect(save).not.toHaveBeenCalled(); expect(props.onGrant).not.toHaveBeenCalled();
  expect(screen.queryByText('All permissions granted')).toBeNull();
  fireEvent.click(screen.getByRole('button', {name:'Skip'}));
  fireEvent.click(await screen.findByRole('button', {name:'Cancel'}));
  expect(save).not.toHaveBeenCalled();
  await skip();
  await waitFor(() => expect(save).toHaveBeenCalledExactlyOnceWith(false));
  expect(props.onDone).toHaveBeenCalledTimes(1);
});

it('confirms an untouched macOS default when leaving', async () => {
  const {view, save} = fixture(); render(view()); await skip();
  await waitFor(() => expect(save).toHaveBeenCalledExactlyOnceWith(true));
});

it('does not apply a draft when the page is closed', () => {
  const {view,save}=fixture(); const {unmount}=render(view());
  fireEvent.click(screen.getByRole('switch')); unmount();
  expect(save).not.toHaveBeenCalled();
});

it('keeps a user draft when a refreshed system snapshot arrives', async () => {
  const {view,props,save}=fixture({...disabled,state:'enabled'});
  const {rerender}=render(view());
  fireEvent.click(screen.getByRole('switch'));
  props.preferences={...props.preferences,launchStatus:{...disabled,state:'enabled',configured:true}};
  rerender(view());
  expect((screen.getByRole('switch') as HTMLInputElement).checked).toBe(false);
  await skip();
  await waitFor(()=>expect(save).toHaveBeenCalledExactlyOnceWith(false));
});

it('preserves a previously confirmed macOS off choice', () => {
  const {view} = fixture({...disabled, configured:true}); render(view());
  expect((screen.getByRole('switch') as HTMLInputElement).checked).toBe(false);
});

it.each([false,true])('excludes the first login row (on=%s) from grant completion and preserves auto-advance timing', async enabled => {
  vi.useFakeTimers();
  const {view, props, save} = fixture(); const {rerender} = render(view());
  if (!enabled) fireEvent.click(screen.getByRole('switch'));
  await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
  expect(save).not.toHaveBeenCalled();
  props.perms = granted; rerender(view());
  expect(screen.getByText('All permissions granted')).toBeTruthy();
  // Editing the switch neither restarts nor cancels the grant-completion timer.
  await act(async () => { await vi.advanceTimersByTimeAsync(100); });
  fireEvent.click(screen.getByRole('switch')); fireEvent.click(screen.getByRole('switch'));
  await act(async () => { await vi.advanceTimersByTimeAsync(80); });
  expect(save).toHaveBeenCalledExactlyOnceWith(enabled);
  expect(props.onDone).toHaveBeenCalledTimes(1);
});

it('waits for applying the value and permits retry after failure', async () => {
  const {view, props, save} = fixture(); save.mockRejectedValueOnce(new Error('offline'));
  render(view()); await skip();
  await waitFor(() => expect(agent.fail).toHaveBeenCalledWith('unavailable'));
  expect(props.onDone).not.toHaveBeenCalled();
  let finish!: () => void;
  save.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve; }));
  await skip();
  expect(props.onDone).not.toHaveBeenCalled();
  await act(async () => { finish(); });
  expect(props.onDone).toHaveBeenCalledTimes(1);
});

it('lets an unavailable development process continue without registering', async () => {
  const {view,props,save}=fixture({state:'unavailable',problem:'main_app_required',configured:false});
  render(view());
  expect((screen.getByRole('switch') as HTMLInputElement).disabled).toBe(true);
  await skip();
  await waitFor(()=>expect(props.onDone).toHaveBeenCalledTimes(1));
  expect(save).not.toHaveBeenCalled();
});

it('does not treat a failed OS query as a confirmed choice', async () => {
  const {view,props,save}=fixture({state:'unavailable',problem:'system_unavailable',configured:false});
  render(view()); await skip();
  await waitFor(()=>expect(agent.fail).toHaveBeenCalledWith('unavailable'));
  expect(props.onDone).not.toHaveBeenCalled(); expect(save).not.toHaveBeenCalled();
});

it('confirms only once when Next races the automatic permission-completion timer', async () => {
  vi.useFakeTimers();
  const {view,props,save}=fixture(); props.perms=granted; render(view());
  fireEvent.click(screen.getByRole('button',{name:'Next'}));
  await act(async()=>{await vi.advanceTimersByTimeAsync(180);});
  expect(save).toHaveBeenCalledExactlyOnceWith(true);
  expect(props.onDone).toHaveBeenCalledTimes(1);
});

it('reads the actual state if the initial status has not arrived', async () => {
  const read = vi.spyOn(api,'launchAtLoginStatus').mockResolvedValue({...disabled,state:'enabled'});
  const {view, save} = fixture(null); render(view());
  expect((screen.getByRole('switch') as HTMLInputElement).disabled).toBe(true);
  await skip();
  await waitFor(() => expect(save).toHaveBeenCalledWith(true));
  read.mockRestore();
});
