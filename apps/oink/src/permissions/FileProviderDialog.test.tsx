// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { App } from '../App';
import { toast } from 'oink-ui';
import { api } from '../api/client';
import { agent } from '../api/store';
import type { FileProviderStatus, Overview } from '../api/types';
import { setLanguage } from '../i18n';

vi.mock('oink-ui', async original => ({ ...await original<typeof import('oink-ui')>(), speed: () => 0 }));
vi.mock('../api/store', async original => ({ ...await original<typeof import('../api/store')>(), start: vi.fn() }));
vi.mock('../api/transport', async original => ({
  ...await original<typeof import('../api/transport')>(),
  transport: { call: async ({ method }: { method: string }) => method === 'group' ? { members: [] } : [], shell: async () => null, onEvent: async () => () => {} },
}));

beforeAll(() => {
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  Element.prototype.animate = vi.fn(() => ({ cancel() {}, finished: Promise.resolve() })) as unknown as typeof Element.prototype.animate;
  HTMLElement.prototype.scrollTo = () => {};
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
let enable: ReturnType<typeof vi.spyOn>;
beforeEach(() => {
  setLanguage('en');
  enable = vi.spyOn(api, 'openFileProviderEnable').mockResolvedValue(null);
  agent.adopt('boot', { platform: 'macos', version: '0.1.0', build: null, backend: 'agent' });
  agent.adopt('fileProvider', { state: 'not_enabled', needed: true, opening: false, detail: null } as never);
  vi.spyOn(api, 'fileProviderStatus').mockImplementation(async () => agent.get().fileProvider!);
});
afterEach(() => { cleanup(); agent.reset(); localStorage.clear(); vi.restoreAllMocks(); });

const TEXT = 'In the Finder window that opens, click Enable in the top-right corner.';
async function answer(button: 'OK' | 'Cancel', before = 0) {
  const dialog = await screen.findByRole('dialog', { name: 'Enable File Provider' });
  expect(within(dialog).getByText(TEXT)).toBeTruthy();
  expect(enable).toHaveBeenCalledTimes(before);
  await act(async () => { fireEvent.click(within(dialog).getByRole('button', { name: button })); });
  await waitFor(() => expect(screen.queryByRole('dialog', { name: 'Enable File Provider' })).toBeNull());
}

describe('File Provider explainer', () => {
  it('reads an already enabled domain on entry and refreshes it on focus/return without enabling it', async () => {
    let source: FileProviderStatus = {state:'not_enabled',needed:false,opening:false,detail:null};
    vi.mocked(api.fileProviderStatus).mockImplementation(async () => source);
    agent.adopt('overview', { this_device: { id: 'self', fingerprint: '0000' }, alerts: [], onboarding_done: false,
      pause: { paused: false, scope: 'local', revision: 1, author: null, author_name: null, emergency: false, capture_detached: false, settling: false } } as unknown as Overview);
    render(<App/>);
    await waitFor(() => expect(api.fileProviderStatus).toHaveBeenCalled());
    const initialReads = vi.mocked(api.fileProviderStatus).mock.calls.length;
    // The domain is already enabled when entering permissions, even if the
    // welcome screen's earlier snapshot still says it was not enabled.
    source = {...source,state:'available'};
    fireEvent.click(await screen.findByRole('button', {name:'Next'}));
    const row = (await screen.findByText('File Provider')).closest<HTMLElement>('.ok-row')!;
    await waitFor(() => expect(within(row).getByText('Granted')).toBeTruthy());
    expect(within(row).queryByRole('button', {name:'Enable'})).toBeNull();
    expect(vi.mocked(api.fileProviderStatus).mock.calls.length).toBeGreaterThan(initialReads);
    expect(enable).not.toHaveBeenCalled();

    source = {...source,state:'not_enabled'};
    fireEvent(window, new Event('focus'));
    await waitFor(() => expect(within(row).getByRole('button', {name:'Enable'})).toBeTruthy());
    source = {...source,state:'available'};
    fireEvent(document, new Event('visibilitychange'));
    await waitFor(() => expect(within(row).getByText('Granted')).toBeTruthy());
    expect(enable).not.toHaveBeenCalled();
    expect(screen.queryByRole('dialog', {name:'Enable File Provider'})).toBeNull();
  });

  it('adopts enabled-domain events after a pending query without an enable click', async () => {
    let finish!: (status: FileProviderStatus) => void;
    vi.mocked(api.fileProviderStatus).mockImplementation(() => new Promise(resolve => { finish = resolve; }));
    render(<App/>);
    await waitFor(() => expect(api.fileProviderStatus).toHaveBeenCalled());
    const enabled: FileProviderStatus = {state:'available',needed:false,opening:false,detail:null};
    act(() => agent.receive({type:'file_provider_status',data:enabled}));
    await act(async () => finish({...enabled,state:'unregistered'}));
    expect(agent.get().fileProvider?.state).toBe('available');
    expect(enable).not.toHaveBeenCalled();
  });

  const settingsButton = async (container: HTMLElement) => {
    fireEvent.click(container.querySelector('[data-action="settings"]')!);
    const row = await waitFor(() => container.querySelector<HTMLElement>('[data-item="perm-files"]')!);
    // `hidden`: jsdom still reports the settings page as hidden while it slides in.
    return within(row).getByRole('button', { name: 'Enable', hidden: true });
  };

  it.each([['OK', 1], ['Cancel', 0]] as const)('asks before Settings opens Finder; %s', async (choice, calls) => {
    const { container } = render(<App/>);
    fireEvent.click(await settingsButton(container));
    await answer(choice);
    expect(enable).toHaveBeenCalledTimes(calls);
    // Asking again works the same way (the second click is not a different action).
    fireEvent.click(await settingsButton(container));
    await answer('OK', calls);
    expect(enable).toHaveBeenCalledTimes(calls + 1);
  });

  it.each([['OK', 1], ['Cancel', 0]] as const)('asks once before onboarding opens Finder; %s', async (choice, calls) => {
    agent.adopt('overview', { this_device: { id: 'self', fingerprint: '0000' }, alerts: [], onboarding_done: false,
      pause: { paused: false, scope: 'local', revision: 1, author: null, author_name: null, emergency: false, capture_detached: false, settling: false } } as unknown as Overview);
    render(<App/>);
    fireEvent.click(await screen.findByRole('button', { name: 'Next' }));
    const row = (await screen.findByText('File Provider')).closest<HTMLElement>('.ok-row')!;
    fireEvent.click(within(row).getByRole('button', { name: 'Enable' }));
    expect(await screen.findAllByRole('dialog', { name: 'Enable File Provider' })).toHaveLength(1);
    await answer(choice);
    expect(enable).toHaveBeenCalledTimes(calls);
  });
});


describe('file publication requires an explicit enable action', () => {
  it.each([
    ['zh-Hans', '需要启用文件提供服务', '启用'],
    ['en', 'Enable File Provider to receive files', 'Enable'],
    ['ja', 'ファイルを受信するにはファイルプロバイダを有効にしてください', '有効にする'],
    ['ko', '파일을 받으려면 파일 제공자를 활성화하세요', '활성화'],
  ] as const)('shows an actionable toast in %s without opening Finder', async (language, title, label) => {
    setLanguage(language);
    const notice = vi.spyOn(toast, 'warning').mockImplementation(() => undefined as never);
    render(<App/>);
    expect(enable).not.toHaveBeenCalled();
    act(() => agent.receive({type: 'file_provider_required'} as never));
    expect(notice).toHaveBeenCalledTimes(1);
    expect(notice.mock.calls[0][0]).toMatchObject({title, action: {label}});
    expect(enable).not.toHaveBeenCalled();
  });

  it('uses the same explainer and enable action as Settings, and never prompts from status alone', async () => {
    const notice = vi.spyOn(toast, 'warning').mockImplementation(() => undefined as never);
    render(<App/>);
    act(() => agent.receive({type:'file_provider_status',data:{state:'not_enabled',needed:true,opening:false,detail:null}}));
    expect(notice).not.toHaveBeenCalled();
    expect(enable).not.toHaveBeenCalled();
    act(() => agent.receive({type:'file_provider_required'} as never));
    const options = notice.mock.calls[0][0];
    if (typeof options === 'string') throw new Error('toast needs an action');
    await act(async () => { void options.action?.onClick?.(); });
    await answer('Cancel');
    expect(enable).not.toHaveBeenCalled();
    await act(async () => { void options.action?.onClick?.(); });
    await answer('OK');
    expect(enable).toHaveBeenCalledTimes(1);
  });
});
