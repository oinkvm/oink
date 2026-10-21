// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, waitFor, within } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { agent } from '../api/store';
import type { InputSettings } from '../api/types';
import { api as backend, shell } from '../api/client';
import { setLanguage } from '../i18n';
import { SettingsPage, type PermState } from './SettingsPage';
import type { Platform } from '../window/DesktopWindow';
import { DEFAULT_PREFS, type PermissionId, type PrefsApi } from './prefs';
import { searchSections, sectionsFor } from './schema';
beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: true, addEventListener() {}, removeEventListener() {} }) });
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  HTMLElement.prototype.scrollTo = () => {};
  vi.spyOn(backend, 'cacheUsage').mockResolvedValue([]);
});
afterEach(cleanup);
const prefs = { prefs: DEFAULT_PREFS, set() {}, canEdit: () => true } as unknown as PrefsApi;
const initial: Record<PermissionId, PermState> = { accessibility: 'not_granted', input: 'not_granted', network: 'not_required', files: 'unknown', audio: 'not_determined' };
function view(perms = initial, query = '', onGrant = vi.fn(), onOpenPermissionSettings?: (id: PermissionId) => void, platform: Platform = 'mac') {
  return <OinkProvider defaultMotion="reduced"><SettingsPage open platform={platform} api={prefs} capsIme={false} query={query} jump={null} onActive={() => {}} version="test" upgrade={{available:false,next:''}} perms={perms} onGrant={onGrant} onOpenPermissionSettings={onOpenPermissionSettings} onLog={() => {}} onAbout={() => {}} onBlocklist={() => {}}/></OinkProvider>;
}
it.each(['en','ja','ko','zh-Hans'])('hides contacts settings without an injector in %s', language => {
  setLanguage(language);
  for (const available of [false,true]) {
    agent.adopt('input', {contacts_injection_available:available} as unknown as InputSettings);
    const {container,unmount}=render(view(initial,'',vi.fn(),undefined,'win'));
    expect(!!container.querySelector('[data-item="touchpadInject"]')).toBe(available);
    unmount();
  }
  agent.adopt('input', null);
});
describe('applicable independent permission rows', () => {
  it.each(['en', 'zh-Hans'] as const)('shows the file recovery fault with a restart instruction in %s', language => {
    setLanguage(language);
    const { container } = render(view({ ...initial, files: 'restart_required' }));
    const row = container.querySelector('[data-item="perm-files"]')!;
    expect(row.textContent).toContain(language === 'en' ? 'File service is unavailable. Reopen Oink.' : '文件服务不可用，请重新打开 Oink');
    expect(row.querySelector('button')).toBeNull();
  });
  it.each(['mac', 'win'] as const)('opens About from the General View button on %s', platform => {
    setLanguage('en');
    const onAbout = vi.fn();
    const { container } = render(<OinkProvider defaultMotion="reduced"><SettingsPage open platform={platform} api={prefs} capsIme={false} query="" jump={null} onActive={() => {}} version="test" upgrade={{available:false,next:''}} perms={initial} onGrant={() => {}} onLog={() => {}} onAbout={onAbout} onBlocklist={() => {}} /></OinkProvider>);
    const row = container.querySelector<HTMLElement>('[data-item="about"]')!;
    fireEvent.click(within(row).getByRole('button', { name: 'View' }));
    expect(onAbout).toHaveBeenCalledOnce();
  });

  it.each(['en', 'zh-Hans'] as const)('opens the crash report privacy notice without changing consent in %s', async language => {
    setLanguage(language);
    const open = vi.spyOn(shell, 'openUrl').mockResolvedValue();
    const set = vi.spyOn(prefs, 'set');
    const { container, rerender } = render(view());
    for (const editable of [true, false]) {
      const canEdit = vi.spyOn(prefs, 'canEdit').mockReturnValue(editable);
      rerender(view());
      const row = container.querySelector('[data-item="crashReports"]')!;
      const link = within(row as HTMLElement).getByRole('link', { name: language === 'en' ? 'Privacy policy' : '隐私政策' });
      expect(link.getAttribute('href')).toBe('https://github.com/oinkvm/oink/blob/main/PRIVACY.md');
      fireEvent.click(link);
      await waitFor(() => expect(open).toHaveBeenCalledWith('https://github.com/oinkvm/oink/blob/main/PRIVACY.md'));
      expect(set).not.toHaveBeenCalled();
      canEdit.mockRestore();
    }
    open.mockRestore();
    set.mockRestore();
  });
  it.each([['mac','en'],['mac','zh-Hans'],['win','en'],['win','zh-Hans']] as const)('has no projection preference or search row on %s in %s', (platform,language) => {
    setLanguage(language);
    const {container,rerender}=render(view(initial,'',vi.fn(),undefined,platform));
    expect(container.querySelector('[data-item="layoutProjection"]')).toBeNull();
    expect(sectionsFor(platform).flatMap(section=>section.items).some(item=>item.id==='layoutProjection')).toBe(false);
    expect(DEFAULT_PREFS).not.toHaveProperty('layoutProjection');
    for(const query of ['Layout projection','屏幕排列投影','layout projection screens edges']) {
      expect(searchSections(platform,query).size).toBe(0);
      rerender(view(initial,query,vi.fn(),undefined,platform));
      expect(container.querySelector('[data-item="layoutProjection"]')).toBeNull();
    }
  });
  it.each(['en', 'zh-Hans'] as const)('does not show installer-managed Windows permission rows in %s', language => {
    setLanguage(language);
    const { container } = render(view(initial, '', vi.fn(), undefined, 'win'));
    expect(container.querySelector('[data-section="permissions"]')).toBeNull();
    expect(container.querySelector('[data-item^="perm-"]')).toBeNull();
  });
  it.each(['en', 'zh-Hans'] as const)('shares onboarding row and button layout in %s', language => {
    setLanguage(language);
    const { container } = render(view({ ...initial, network: 'not_determined', files: 'not_granted' }));
    const rows = container.querySelectorAll('[data-item^="perm-"]');
    expect(rows.length).toBe(5);
    for (const row of rows) {
      expect(row.classList.contains('permission-row')).toBe(true);
      const button = row.querySelector('button');
      expect(button?.classList.contains('permission-button')).toBe(true);
    }
  });
  it.each(['en', 'zh-Hans'] as const)('omits macOS 14 Local Network from Settings and search in %s', language => {
    setLanguage(language);
    const {container, rerender} = render(view());
    expect(container.querySelector('[data-item="perm-network"]')).toBeNull();
    expect(container.querySelector('[data-item="perm-input"]')).not.toBeNull();
    rerender(view(initial, language === 'en' ? 'Local Network' : '本地网络'));
    expect(container.querySelector('[data-section="permissions"]')).toBeNull();
    rerender(view({...initial, network:'not_determined'}));
    expect(container.querySelector('[data-item="perm-network"]')).not.toBeNull();
  });
  it('updates IM while AX stays ungranted, then independently updates network and audio', () => {
    setLanguage('en');
    const {container, rerender} = render(view({...initial, network:'not_determined'}));
    rerender(view({...initial, input:'granted', network:'not_determined'}));
    expect(container.querySelector('[data-item="perm-input"]')?.textContent).toContain('Granted');
    expect(container.querySelector('[data-item="perm-accessibility"]')?.textContent).toContain('Grant permission');
    rerender(view({...initial, input:'granted', network:'granted', audio:'granted'}));
    expect(container.querySelector('[data-item="perm-network"]')?.textContent).toContain('Granted');
    expect(container.querySelector('[data-item="perm-audio"]')?.textContent).toContain('Granted');
  });
  it('requests one permission first, then offers a separate explicit Settings fallback', async () => {
    setLanguage('en');
    const request = vi.fn(); const settings = vi.fn();
    const { container } = render(view(initial, '', request, settings));
    const button = container.querySelector('[data-item="perm-audio"] button')!;
    fireEvent.click(button);
    expect(request.mock.calls).toEqual([['audio']]);
    expect(settings).not.toHaveBeenCalled();
    await waitFor(() => expect(button.textContent).toContain('Open Settings'));
    fireEvent.click(button);
    expect(settings.mock.calls).toEqual([['audio']]);
    expect(request).toHaveBeenCalledTimes(1);
  });

});


describe('settings actions', () => {
  it.each(['en','zh-Hans'] as const)('opens Releases and hides the update notice setting in %s', async language => {
    setLanguage(language);
    const open = vi.spyOn(shell, 'openUrl').mockResolvedValue();
    const { container } = render(view());
    expect(container.querySelector('[data-item="upgradeHint"]')).toBeNull();
    fireEvent.click(container.querySelector('[data-item="version"] button')!);
    await waitFor(() => expect(open).toHaveBeenCalledWith('https://github.com/oinkvm/oink/releases'));
    const input = container.querySelector('[data-item="logMaxMb"] input')!;
    expect(input.closest('.ok-input')?.textContent).toContain('MB');
    open.mockRestore();
  });
  it.each(['en','zh-Hans'] as const)('opens the diagnostics folder and saves each offered range in %s', async language => {
    setLanguage(language);
    const open = vi.spyOn(shell, 'openDiagnosticsFolder').mockResolvedValue();
    const save = vi.spyOn(backend, 'exportDiagnostics').mockResolvedValue({ path:'bundle.zip',bytes:1,cutoff_complete:true,included_device_count:3,missing_device_count:0 });
    const { container } = render(view());
    expect(container.querySelector('[data-item="bundleDir"]')).toBeNull();
    const row = container.querySelector('[data-item="bundle"]') as HTMLElement;
    fireEvent.click(within(row).getByRole('button',{name:language==='en'?'Open':'打开'}));
    expect(open).toHaveBeenCalledOnce();
    const labels = language==='en'?['Last 30 minutes','Last 2 hours','All']:['最近 30 分钟','最近 2 小时','全部'];
    for (const [i,range] of [30,120,null].entries()) {
      const trigger = within(row).getByRole('button',{name:language==='en'?'Save':'保存'});
      await waitFor(()=>expect((trigger as HTMLButtonElement).disabled).toBe(false));
      fireEvent.click(trigger);
      fireEvent.click(await within(document.body).findByRole('menuitem',{name:labels[i]}));
      await waitFor(()=>expect(save).toHaveBeenLastCalledWith(range));
      await waitFor(()=>expect((trigger as HTMLButtonElement).disabled).toBe(false));
    }
    open.mockRestore();save.mockRestore();
  });
});

describe('renamed setting search aliases',()=>{
  it.each([
    ['zh-Hans','同步私有格式','同步应用专有格式','本机保持屏幕点亮','保持屏幕常亮','音频录制','系统音频录制','诊断包保存位置'],
    ['en','Sync private formats','Sync app-specific formats','Keep this screen awake','Keep this screen awake','Audio recording','System Audio Recording','Diagnostics folder'],
    ['ja','独自フォーマットを同期','アプリ独自の形式を同期','このデバイスの画面をオンのままにする','画面をオンのままにする','システムオーディオ録音','システムオーディオの録音','診断パッケージの保存場所'],
    ['ko','고유 형식 동기화','앱 전용 형식 동기화','이 기기의 화면 켜 두기','화면 켜 두기','오디오 녹음','시스템 오디오 녹음','진단 패키지 저장 위치'],
  ])('finds both old and new names in %s',(language,oldFormats,newFormats,oldAwake,newAwake,oldAudio,newAudio,folder)=>{
    setLanguage(language);
    for(const [id,section,queries] of [['clipPrivate','clipboard',[oldFormats,newFormats]],['keepAwake','input',[oldAwake,newAwake]],['perm-audio','permissions',[oldAudio,newAudio]],['bundle','diagnostics',[folder]]] as const){
      for(const query of queries){const found=searchSections('mac',query).get(section);expect(found==='all'||found?.has(id)).toBe(true);}
    }
  });
});

it.each([
  ['zh-Hans','长按 Esc 5 秒暂停全组共享','暂停全组共享','卸载 Oink…'],
  ['en','Hold Esc for 5 seconds to pause group sharing','Pause group sharing','Uninstall Oink…'],
  ['ja','Esc を 5 秒間長押ししてグループ全体の共有を一時停止','グループ全体の共有を一時停止','Oink をアンインストール…'],
  ['ko','Esc 키를 5초간 길게 눌러 그룹 전체 공유 일시 정지','그룹 전체 공유 일시 정지','Oink 제거…'],
])('retains complete former names in %s',(lang,esc,pause,uninstall)=>{
  setLanguage(lang);
  for(const [query,id,section] of [[esc,'escStop','general'],[pause,'sc-pauseAll','shortcuts'],[uninstall,'uninstall','general']] as const){
    const found=searchSections('mac',query).get(section);expect(found==='all'||found?.has(id)).toBe(true);
  }
});
