// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { DEFAULT_ACCENT } from '../settings/appearance';
import { setLanguage } from '../i18n';
import { api, shell } from '../api/client';
import { pairingApi } from '../api/pairing';
import type { Pairing } from '../pairing/usePairing';
import type { PrefsApi } from '../settings/prefs';
import { Onboarding, OnboardingLanguage, type OnboardingProps } from './Onboarding';

vi.mock('oink-ui', async importOriginal => ({ ...await importOriginal<typeof import('oink-ui')>(), speed: () => 0 }));

vi.mock('../api/store', () => ({
  useAgent: () => ({ network: { addresses: [{ ip: '192.0.2.1' }] }, overview: { this_device: { fingerprint: '0000-0000-0000' } } }),
  agent: { fail: vi.fn(), query: vi.fn().mockResolvedValue(undefined) },
}));
vi.mock('../api/pairing', () => ({ pairingApi: { onboardingConnect: vi.fn().mockResolvedValue(null), pairings: vi.fn() } }));
beforeAll(() => {
  Element.prototype.animate = vi.fn(() => ({ cancel() {} })) as unknown as typeof Element.prototype.animate;
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: true, addEventListener() {}, removeEventListener() {} }) });
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
});
beforeEach(() => { setLanguage('en'); vi.restoreAllMocks(); vi.mocked(pairingApi.onboardingConnect).mockResolvedValue(null); });
afterEach(cleanup);
function fixture(extras: Partial<OnboardingProps> = {}) {
  const actions = { accept: vi.fn(), cancel: vi.fn().mockResolvedValue(undefined), dismiss: vi.fn(), exited: vi.fn(), connect: vi.fn(), refresh: vi.fn(), setInput: vi.fn(), submit: vi.fn(), search: vi.fn(), pairFound: vi.fn() };
  const props: OnboardingProps = {
    platform: 'win', pairing: { requests: [], nearby: [], dialog: { open: false, mode: 'manual', session: null, key: 0 }, actions, defaultPort: 47470 } as unknown as Pairing,
    perms: { accessibility: 'not_granted', input: 'not_granted', network: 'not_required', files: 'denied', audio: 'not_determined' },
    onGrant: vi.fn(), onOpenSettings: vi.fn(), container: null, onSkip: vi.fn(), onFinish: vi.fn(), leaving: false, connected: false, version: '0.1.0-beta.1',
    preferences: { launchStatus: {state:'disabled',problem:null,configured:false}, confirmLaunchAtLogin: vi.fn().mockResolvedValue(undefined), ready: true, report: {}, chooseReport: vi.fn().mockResolvedValue(undefined), saveLocal: vi.fn().mockResolvedValue(undefined) } as unknown as PrefsApi,
    ...extras,
  };
  const view = (p = props) => <OinkProvider defaultAccent={DEFAULT_ACCENT.value} defaultMotion="reduced"><Onboarding {...p}/></OinkProvider>;
  return { props, actions, view };
}
async function next() { fireEvent.click(screen.getByRole('button', { name: 'Next' })); }
async function telemetry() { await next(); await screen.findByRole('switch', {name:'Launch at login'}); await next(); await screen.findByText('Help us improve Oink'); }
async function connect() { await telemetry(); await next(); await screen.findByText('Connect your other devices'); }

it('persists only explicit accent and appearance edits from the welcome page', async () => {
  const { props, view } = fixture(); render(view());
  expect(screen.getByRole('radio', { name: 'Pink' }).getAttribute('aria-checked')).toBe('true');
  expect(props.preferences.saveLocal).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('radio', { name: 'Dark' }));
  await waitFor(() => expect(props.preferences.saveLocal).toHaveBeenCalledWith({ theme_mode: 'dark' }));
  fireEvent.click(screen.getByRole('radio', { name: 'Blue' }));
  expect(props.preferences.saveLocal).toHaveBeenCalledWith({ theme_accent: { light: '#2E7BFF', dark: '#2E7BFF' } });
});

it.each(['en', 'zh-Hans'] as const)('shows only the crash-report row and opens privacy without changing consent in %s', async language => {
  setLanguage(language); const open = vi.spyOn(shell, 'openUrl').mockResolvedValue();
  const { props, view } = fixture(); render(view());
  fireEvent.click(screen.getByRole('button', { name: language === 'en' ? 'Next' : '下一步' }));
  await screen.findByRole('switch', {name: language === 'en' ? 'Launch at login' : '开机自动启动'});
  fireEvent.click(screen.getByRole('button', { name: language === 'en' ? 'Next' : '下一步' }));
  const privacy = await screen.findByRole('link', { name: language === 'en' ? 'View privacy policy' : '查看隐私政策' });
  expect(document.querySelectorAll('.ob-telemetry__card .ok-row')).toHaveLength(1);
  expect(screen.queryByText(language === 'en' ? 'Support Oink development' : '赞助 Oink 的开发')).toBeNull();
  expect(screen.queryByRole('button', { name: language === 'en' ? 'WeChat Pay' : '微信支付' })).toBeNull();
  expect(screen.queryByRole('link', { name: 'Ko-fi' })).toBeNull();
  expect(screen.queryByRole('dialog')).toBeNull();
  expect(document.body.textContent).toContain(language === 'en' ? 'Sentry (EU data centre)' : 'Sentry（欧盟数据中心）');
  expect((screen.getByRole('switch') as HTMLInputElement).checked).toBe(false);
  fireEvent.click(privacy);
  expect(open).toHaveBeenCalledWith('https://github.com/oinkvm/oink/blob/main/PRIVACY.md');
  expect(props.preferences.chooseReport).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: language === 'en' ? 'Next' : '下一步' }));
  await waitFor(() => expect(props.preferences.chooseReport).toHaveBeenCalledWith(false));
});

it('waits for consent persistence before continuing, and lets a failed save be retried', async () => {
  const { props, view } = fixture();
  vi.mocked(props.preferences.chooseReport).mockRejectedValueOnce(new Error('offline'));
  render(view()); await telemetry();
  fireEvent.click(screen.getByRole('switch')); await next();
  await waitFor(() => expect(props.preferences.chooseReport).toHaveBeenCalledWith(true));
  expect(screen.queryByText('Connect your other devices')).toBeNull();
  await waitFor(() => expect((screen.getByRole('button', { name: 'Next' }) as HTMLButtonElement).disabled).toBe(false));
  await next(); await screen.findByText('Connect your other devices');
});

it('omits inapplicable permissions and reuses the request then Settings grant flow', async () => {
  const { props, view } = fixture({ platform: 'mac' }); render(view()); await next();
  await screen.findByText('Grant the required permissions');
  expect(screen.queryByText('Local Network')).toBeNull();
  const grant = screen.getAllByRole('button', { name: 'Grant permission' })[0];
  fireEvent.click(grant); await waitFor(() => expect(props.onGrant).toHaveBeenCalledWith('accessibility'));
  fireEvent.click(await screen.findByRole('button', { name: 'Open Settings' }));
  expect(props.onOpenSettings).toHaveBeenCalledWith('accessibility');
  fireEvent.click(screen.getByRole('button', { name: 'Skip' }));
  await screen.findByText('Skip remaining permissions');
  fireEvent.click(screen.getByRole('button', { name: 'Confirm' }));
  await screen.findByText('Help us improve Oink');
});

it.each(['en', 'zh-Hans'] as const)('explains that granting audio permission keeps playback local in %s', async language => {
  setLanguage(language);
  const { props, view } = fixture({ platform: 'mac' });
  render(view());
  fireEvent.click(screen.getByRole('button', { name: language === 'en' ? 'Next' : '下一步' }));
  const forwarding = vi.spyOn(api, 'setAudioSettings');
  const description = await screen.findByText(language === 'en' ? 'Audio stays local by default. This permission only allows forwarding when you turn it on.' : '默认在本机播放；此权限仅允许你手动开启音频转发');
  expect(props.onGrant).not.toHaveBeenCalled();
  fireEvent.click(description.closest('.permission-row')!.querySelector('button')!);
  await waitFor(() => expect(props.onGrant).toHaveBeenCalledExactlyOnceWith('audio'));
  expect(forwarding).not.toHaveBeenCalled();
  expect(props.preferences.saveLocal).not.toHaveBeenCalled();
});

it('confirms login when grants are ready and skips Connect when already connected', async () => {
  const { props, view } = fixture({ platform: 'mac', connected: true, perms: { accessibility: 'granted', input: 'granted', network: 'not_required', files: 'unsupported', audio: 'granted' } });
  render(view()); await next(); await screen.findByText('Help us improve Oink'); await next();
  expect(props.preferences.confirmLaunchAtLogin).toHaveBeenCalledWith(true);
  await waitFor(() => expect(props.onFinish).toHaveBeenCalledTimes(1));
  expect(screen.queryByText('Connect your other devices')).toBeNull();
});

it('lets completion persistence be retried when Connect is skipped', async () => {
  const finish = vi.fn().mockResolvedValueOnce(false).mockResolvedValue(true);
  const { view } = fixture({ connected: true, onFinish: finish });
  render(view()); await telemetry(); await next();
  await waitFor(() => expect(finish).toHaveBeenCalledTimes(1));
  await next(); await waitFor(() => expect(finish).toHaveBeenCalledTimes(2));
});

it('accepts an incoming request once, renders code entry in place, and returns without cancelling it', async () => {
  const { props, view, actions } = fixture();
  const request = { id: 'request-1', name: 'Remote', deviceId: 'AAAA-BBBB-CCCC' } as Pairing['requests'][number];
  props.pairing.requests = [request];
  const { rerender } = render(view()); await connect();
  await waitFor(() => expect(actions.accept).toHaveBeenCalledExactlyOnceWith(request));
  props.pairing = { ...props.pairing, dialog: { open: true, key: 1, mode: 'pair', session: { key: 1, pairId: request.id, peer: request, dir: 'in', source: 'request', code: '', input: '', status: 'input', wrongs: 0, sourceCount: null, targetCount: null } } };
  rerender(view());
  expect(screen.getByRole('heading', { name: 'Pair with Remote' })).toBeTruthy();
  expect(screen.getAllByRole('textbox')).toHaveLength(6);
  fireEvent.click(screen.getByRole('button', { name: 'Back' }));
  expect(actions.dismiss).toHaveBeenCalledTimes(1); expect(actions.cancel).not.toHaveBeenCalled();
  props.pairing = { ...props.pairing, dialog: { ...props.pairing.dialog, open: false, session: null } }; rerender(view());
  expect(actions.accept).toHaveBeenCalledTimes(1);
  expect(screen.getByText('192.0.2.1')).toBeTruthy(); expect(screen.getByText('0000-0000-0000')).toBeTruthy();
});

it('selects the real language preference from the top-right menu', async () => {
  const change = vi.fn();
  render(<OinkProvider defaultMotion="reduced"><OnboardingLanguage value="auto" onChange={change} leaving={false}/></OinkProvider>);
  fireEvent.click(screen.getByRole('button', { name: 'Language: System' }));
  fireEvent.click(await screen.findByRole('menuitemradio', { name: /English/ }));
  expect(change).toHaveBeenCalledWith('en');
});

it('waits for backend admission policy before auto-accepting on Connect', async () => {
  let activate!: () => void;
  const gate = new Promise<null>(resolve => { activate = () => resolve(null); });
  vi.mocked(pairingApi.onboardingConnect).mockImplementation(active => active ? gate : Promise.resolve(null));
  const { props, view, actions } = fixture();
  const request = { id: 'first', name: 'Remote', deviceId: 'AAAA-BBBB-CCCC' } as Pairing['requests'][number];
  props.pairing.requests = [request];
  render(view()); await connect();
  expect(actions.accept).not.toHaveBeenCalled();
  expect(document.querySelector('.ob-connect .ob-main')?.hasAttribute('inert')).toBe(true);
  await act(async () => { activate(); });
  await waitFor(() => expect(actions.accept).toHaveBeenCalledExactlyOnceWith(request));
  expect(document.querySelector('.ob-connect .ob-main')?.hasAttribute('inert')).toBe(false);
});

it.each(['input', 'wrong', 'verifying', 'failed'] as const)('replaces only a terminal failed pairing, current=%s', async status => {
  const { props, actions, view } = fixture();
  const { rerender, unmount } = render(view()); await connect();
  await waitFor(() => expect(pairingApi.onboardingConnect).toHaveBeenCalledWith(true));
  const old = { id: 'old', name: 'Old peer', deviceId: 'AAAA-BBBB-CCCC' } as Pairing['requests'][number];
  const next = { id: 'new', name: 'New peer', deviceId: 'DDDD-EEEE-FFFF' } as Pairing['requests'][number];
  props.pairing = { ...props.pairing, requests: [next], dialog: { open: true, key: 1, mode: 'pair', session: { key: 1, pairId: old.id, peer: old, dir: 'in', source: 'request', code: '', input: '', status, wrongs: 0, sourceCount: null, targetCount: null } } };
  rerender(view());
  await act(async () => {});
  if (status === 'failed') {
    await waitFor(() => expect(actions.accept).toHaveBeenCalledExactlyOnceWith(next));
    props.pairing = { ...props.pairing, requests: [], dialog: { ...props.pairing.dialog, key: 2, session: { ...props.pairing.dialog.session!, key: 2, pairId: next.id, peer: next, status: 'connecting' } } };
    rerender(view());
    expect(document.querySelector('.ob-pair__title')?.textContent).toBe('Pair with Old peer');
    props.pairing = { ...props.pairing, dialog: { ...props.pairing.dialog, session: { ...props.pairing.dialog.session!, status: 'input' } } };
    rerender(view());
    expect(screen.getByRole('heading', { name: 'Pair with New peer' })).toBeTruthy();
    expect(screen.getAllByRole('textbox')).toHaveLength(6);
    expect(screen.queryByRole('heading', { name: 'Pair with Old peer' })).toBeNull();
    expect(actions.cancel).not.toHaveBeenCalled();
  } else expect(actions.accept).not.toHaveBeenCalled();
  unmount();
  await waitFor(() => expect(pairingApi.onboardingConnect).toHaveBeenLastCalledWith(false));
});
