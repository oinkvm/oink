// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor, within } from '@testing-library/react';
import { OinkProvider, Toaster, toast } from 'oink-ui';
import { agent } from '../api/store';
import type { Overview } from '../api/types';
import { api as backend, shell } from '../api/client';
import { setLanguage } from '../i18n';
import { SettingsPage, type PermState } from './SettingsPage';
import { DEFAULT_PREFS, type PermissionId, type PrefsApi } from './prefs';
import { searchSections } from './schema';
import { createLayoutProfilesFixture } from '../api/layoutProfiles.fixture';
import type { UpdateStatus } from '../api/update';
beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: true, addEventListener() {}, removeEventListener() {} }) });
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  HTMLElement.prototype.scrollTo = () => {};
  HTMLElement.prototype.scrollIntoView = () => {};
});
afterEach(() => { toast.dismiss(); cleanup(); vi.restoreAllMocks(); agent.adopt('overview', null); agent.adopt('receivedFiles',null); });
const prefs = { prefs: DEFAULT_PREFS, set() {}, canEdit: () => true } as unknown as PrefsApi;
const perms: Record<PermissionId, PermState> = { accessibility: 'granted', input: 'granted', network: 'not_required', files: 'granted', audio: 'granted' };
const view = () => <OinkProvider defaultMotion="reduced"><SettingsPage open platform="mac" api={prefs} capsIme={false} query="" jump={null} onActive={() => {}} version="test" upgrade={{available:false,next:''}} perms={perms} onGrant={() => {}} onLog={() => {}} onAbout={() => {}} onBlocklist={() => {}}/><Toaster/></OinkProvider>;
const usage = (total: number) => ({ categories: [], total_bytes: total, incomplete: true });

it.each(['en', 'zh-Hans', 'ja', 'ko'] as const)('shows cached files without an incomplete note or an open-folder button in %s', async language => {
  setLanguage(language);
  agent.adopt('overview', { this_device: { id: 'self' } } as unknown as Overview);
  vi.spyOn(backend, 'cacheUsage').mockResolvedValue([{ device: 'self', usage: usage(2000) }]);
  const clear = vi.spyOn(backend, 'clearCache').mockResolvedValue({ device: 'self', result: { removed_bytes: 1000, kept_bytes: 1000, usage: usage(1000) } });
  const { container } = render(view());
  const row = container.querySelector<HTMLElement>('[data-item="cache"]')!;
  await waitFor(() => expect(row.textContent).toContain('2.0 KB'));
  expect(within(row).getAllByRole('button')).toHaveLength(1);
  expect(row.textContent).not.toMatch(/incomplete|统计未完成|集計|집계/);
  fireEvent.click(within(row).getByRole('button'));
  await waitFor(() => expect(clear).toHaveBeenCalledOnce());
  const kept = { en: 'Kept 1.0 KB', 'zh-Hans': '保留 1.0 KB', ja: '1.0 KB', ko: '1.0 KB' }[language];
  await waitFor(() => expect(row.querySelector('.ok-row__desc')?.textContent).toContain(kept));
  expect(row.textContent).not.toMatch(/incomplete|统计未完成|集計|집계/);
});

it.each([
  ['en', 'Automatically check for updates', 'Checks for new versions in the background and lets you know when one is available.'],
  ['zh-Hans', '自动检查更新', '在后台定时检查新版本并提示升级'],
] as const)('titles the update row by its switch in %s', (language, title, description) => {
  setLanguage(language);
  const status = { current_build: 1, version: '1', checks_enabled: true, phase: 'idle', candidate: null, checked_at: null, next_check: null, deferred_until: null, blockers: [], progress: null, error: null, scope: 'user', can_change_policy: true } as const;
  agent.adopt('update', status as unknown as UpdateStatus);
  const { container, rerender } = render(view());
  const row = container.querySelector<HTMLElement>('[data-item="updates"]')!;
  expect(row.querySelector('.ok-row__label')?.textContent).toBe(title);
  expect(row.querySelector('.ok-row__desc')?.textContent).toBe(description);
  expect(row.classList.contains('ok-row--fill')).toBe(false);
  expect(within(row).getByRole('switch', { name: title }).closest('.ok-row__text')).toBeNull();
  expect(row.querySelector('label:not(.ok-switch)')).toBeNull();
  agent.adopt('update', { ...status, phase: 'disabled' } as unknown as UpdateStatus);
  rerender(view());
  expect(within(row).queryByRole('switch')).toBeNull();
  expect(row.querySelector('.ok-row__desc')?.textContent).toBe(language === 'en' ? 'Download and install updates manually for this version.' : '此版本需要手动下载并安装更新。');
  agent.adopt('update', null);
});

it.each([['en', 'Software updates'], ['en', 'background'], ['zh-Hans', '软件更新'], ['ja', 'ソフトウェアアップデート'], ['ko', '소프트웨어 업데이트']] as const)('finds the update row by %s keyword %s', (language, query) => {
  setLanguage(language);
  const found = searchSections('mac', query).get('general');
  expect(found === 'all' || found?.has('updates')).toBe(true);
});

it('keeps the per-screen pointer speeds collapsed until the panel is opened', () => {
  setLanguage('en');
  const { container } = render(view());
  const panel = container.querySelector<HTMLElement>('[data-item="screenPointerGains"]')!;
  const head = within(panel).getByRole('button', { name: /Remote pointer speed per screen/ });
  expect(head.getAttribute('aria-expanded')).toBe('false');
  fireEvent.click(head);
  expect(head.getAttribute('aria-expanded')).toBe('true');
  expect(within(panel).getByRole('region').textContent).toContain('Unavailable');
});

it.each([['saves', true], ['fails to save', false]] as const)('offers to open the diagnostics folder when it %s', async (_, ok) => {
  setLanguage('en');
  const open = vi.spyOn(shell, 'openDiagnosticsFolder').mockResolvedValue();
  const save = vi.spyOn(backend, 'exportDiagnostics');
  if (ok) save.mockResolvedValue({ path: 'bundle.zip', bytes: 1, cutoff_complete: true, included_device_count: 2, missing_device_count: 0 });
  else save.mockRejectedValue(new Error('failed'));
  const { container } = render(view());
  const row = container.querySelector<HTMLElement>('[data-item="bundle"]')!;
  expect(within(row).getByRole('button', { name: 'Open' }).querySelector('.ok-icon')).not.toBeNull();
  fireEvent.click(within(row).getByRole('button', { name: 'Save' }));
  fireEvent.click(await within(document.body).findByRole('menuitem', { name: 'Last 30 minutes' }));
  const title = await within(document.body).findByText(ok ? 'Diagnostics saved' : 'Could not save diagnostics');
  const item = title.closest<HTMLElement>('.ok-toast')!;
  fireEvent.click(within(item).getByRole('button', { name: 'Open' }));
  expect(open).toHaveBeenCalledOnce();
});

describe('Layout section', () => {
  const self = 'a'.repeat(64);
  const arrangement = (availability: 'ready' | 'unsupported' | 'unavailable') => {
    const devices = createLayoutProfilesFixture().state().devices.map(d => d.device === self ? { ...d, availability } : d);
    const fixture = createLayoutProfilesFixture({ devices });
    agent.adopt('overview', { this_device: { id: self } } as unknown as Overview);
    agent.adopt('layoutProfiles', fixture.state());
    return fixture;
  };
  afterEach(() => agent.adopt('layoutProfiles', null));

  it('sits directly below Appearance', () => {
    setLanguage('en');
    const { container } = render(view());
    const ids = Array.from(container.querySelectorAll<HTMLElement>('[data-section]'), s => s.dataset.section);
    expect(ids.slice(0, 3)).toEqual(['general', 'appearance', 'layout']);
    const section = container.querySelector<HTMLElement>('[data-section="layout"]')!;
    expect(within(section).getByRole('heading', { level: 2 }).textContent).toBe('Layout');
    expect(Array.from(section.querySelectorAll<HTMLElement>('[data-item]'), i => i.dataset.item)).toEqual(['manageLayouts', 'syncSystemArrangement', 'pointerGain', 'screenPointerGains']);
    const input = container.querySelector<HTMLElement>('[data-section="input"]')!;
    expect(input.querySelector('[data-item="pointerGain"], [data-item="screenPointerGains"]')).toBeNull();
  });

  it('turns system arrangement sync on for this device with its preference token', async () => {
    setLanguage('en');
    const fixture = arrangement('ready');
    const set = vi.spyOn(backend, 'setSyncSystemArrangement').mockImplementation(request => fixture.api.setSyncSystemArrangement(request));
    const { container } = render(view());
    const row = container.querySelector<HTMLElement>('[data-item="syncSystemArrangement"]')!;
    expect(row.querySelector('.ok-row__desc')?.textContent).toBe('When this device has more than one display, changes on the layout page also apply to the system display arrangement');
    expect(row.querySelector('.ok-row__label')?.textContent).toBe("Allow changing this computer's display arrangement");
    const toggle = within(row).getByRole('switch', { name: "Allow changing this computer's display arrangement" }) as HTMLInputElement;
    expect(toggle.checked).toBe(false);
    expect(toggle.disabled).toBe(false);
    fireEvent.click(toggle);
    await waitFor(() => expect(toggle.checked).toBe(true));
    expect(set).toHaveBeenCalledWith(expect.objectContaining({ device: self, enabled: true, expected_preference_token: `preference:${self}:1` }));
  });

  it('hides the switch where the owner cannot write the arrangement and disables it while unavailable', () => {
    setLanguage('en');
    arrangement('unsupported');
    const { container, unmount } = render(view());
    expect(container.querySelector('[data-item="syncSystemArrangement"]')).toBeNull();
    expect(container.querySelector('[data-item="manageLayouts"]')).not.toBeNull();
    unmount();
    arrangement('unavailable');
    const again = render(view()).container.querySelector<HTMLElement>('[data-item="syncSystemArrangement"]')!;
    expect((within(again).getByRole('switch') as HTMLInputElement).disabled).toBe(true);
  });

  it.each([['zh-Hans', '允许修改本机系统屏幕排列'], ['en', "Allow changing this computer's display arrangement"], ['ja', 'このコンピュータのディスプレイ配置の変更を許可'], ['ko', '이 컴퓨터의 디스플레이 정렬 변경 허용']] as const)('names the arrangement permission row in %s', (language, label) => {
    setLanguage(language);
    arrangement('ready');
    const { container } = render(view());
    expect(container.querySelector('[data-item="syncSystemArrangement"] .ok-row__label')?.textContent).toBe(label);
  });

  // The setting's former name stays a search alias.
  it.each([['zh-Hans', '同步调整系统屏幕排列'], ['en', 'Sync system display arrangement'], ['ja', 'システムのディスプレイ配置を同期'], ['ko', '시스템 디스플레이 정렬 동기화']] as const)('finds the permission row by its former %s name', (language, query) => {
    setLanguage(language);
    expect(searchSections('mac', query).get('layout')).toEqual(new Set(['syncSystemArrangement']));
  });

  it.each([['en', 'system display arrangement'], ['zh-Hans', '系统设置'], ['ja', 'ディスプレイ'], ['ko', '레이아웃']] as const)('finds the Layout rows by %s keyword %s', (language, query) => {
    setLanguage(language);
    expect(searchSections('mac', query).get('layout')).toBeDefined();
  });
});

it.each(['en','zh-Hans','ja','ko'] as const)('received files have one folder action with an icon and received-time policy in %s',async language=>{
  setLanguage(language);
  vi.spyOn(backend,'receivedFilesUsage').mockResolvedValue({materialized_bytes:2000,incomplete:false,time_basis:'received',cleanup:'never'});
  const open=vi.spyOn(backend,'openReceivedFiles').mockResolvedValue(null);
  vi.spyOn(backend,'cacheUsage').mockResolvedValue([]);
  const {container}=render(view());
  const row=container.querySelector<HTMLElement>('[data-item="receivedFiles"]')!;
  await waitFor(()=>expect(row.textContent).toContain('2.0 KB'));
  const buttons=within(row).getAllByRole('button');expect(buttons).toHaveLength(1);
  expect(buttons[0].querySelector('svg')).not.toBeNull();
  fireEvent.click(buttons[0]);
  await waitFor(()=>expect(open).toHaveBeenCalledTimes(1));
  const policy=container.querySelector<HTMLElement>('[data-item="receivedCleanup"]')!;
  expect(policy).not.toBeNull();
  expect(row.compareDocumentPosition(policy)&Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(policy.compareDocumentPosition(container.querySelector('[data-item="cache"]')!)&Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
});

it.each([['received','收到 7 天后'],['last_used','7 天未使用']] as const)('received cleanup uses %s age and persists an explicit local choice',async(basis,label)=>{
  setLanguage('zh-Hans');
  const state={materialized_bytes:0,incomplete:false,time_basis:basis,cleanup:'never' as const};
  const usage=vi.spyOn(backend,'receivedFilesUsage').mockResolvedValue(state);
  const save=vi.spyOn(backend,'setReceivedFilesCleanup').mockImplementation(async cleanup=>{usage.mockResolvedValue({...state,cleanup});return null;});
  vi.spyOn(backend,'cacheUsage').mockResolvedValue([]);
  const {container}=render(view());
  const row=container.querySelector<HTMLElement>('[data-item="receivedCleanup"]')!;
  const select=within(row).getByRole('combobox');
  await waitFor(()=>expect((select as HTMLButtonElement).disabled).toBe(false));
  fireEvent.click(select);
  fireEvent.click(await within(document.body).findByRole('option',{name:label}));
  await waitFor(()=>expect(save).toHaveBeenCalledWith('days7'));
  await waitFor(()=>expect(select.textContent).toContain(label));
});
it.each([['en','received files'],['zh-Hans','收到'],['ja','受信'],['ko','받은']] as const)('received rows are searchable in %s', (language,query)=>{
 setLanguage(language);const found=searchSections('mac',query).get('clipboard');expect(found==='all'||found?.has('receivedFiles')).toBe(true);
});
