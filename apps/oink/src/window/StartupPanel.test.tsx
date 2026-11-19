// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { setLanguage } from '../i18n';
import { transport } from '../api/transport';
import { StartupPanel } from './StartupPanel';

afterEach(() => { cleanup(); vi.restoreAllMocks(); });
const cases = [
  ['waiting: CreateFileW os error 231','正在等待 Oink 服务启动…','Waiting for the Oink service to start…',false,false],
  ['startup_missing: 1060','Oink 服务没有安装',"The Oink service isn't installed",false,true],
  ['startup_stopped: 1067','Oink 服务已停止','The Oink service is stopped',true,false],
  ['startup_disabled: 1058','Oink 服务已被禁用','The Oink service is disabled',true,false],
  ['startup_session: WTSDisconnected','请在这台电脑前登录后使用','Sign in at this PC to use Oink',true,false],
  ['oink_update_in_progress','上次更新没有完成',"The last update didn't finish",true,true],
  ['rdp','远程桌面期间共享已暂停','Sharing is paused during Remote Desktop',false,false],
  ['backend_exited:123','Oink 后台启动后意外退出',"Oink's background process quit unexpectedly",true,false],
  ['input service identity verification failed','Oink 服务身份验证失败','Oink service identity verification failed',false,false],
] as const;
for (const zh of [true,false]) {
  it.each(cases)('renders %s and discloses its raw diagnostic on demand (' + (zh?'zh':'en') + ')', async (raw, cn, en, retry, download) => {
    setLanguage(zh ? 'zh-Hans' : 'en');
    vi.spyOn(transport,'shell').mockResolvedValue('SCM sample');
    const {container}=render(<StartupPanel raw={raw}/>);
    expect(screen.getByRole('heading',{name:zh?cn:en})).toBeTruthy();
    expect(container.querySelector('.ok-empty__desc')?.textContent).not.toMatch(/[。.]$/);
    expect(Boolean(screen.queryByText(zh?'重试':'Retry'))).toBe(retry);
    expect(Boolean(screen.queryByText(zh?'下载安装包':'Download installer'))).toBe(download);
    expect(container.querySelector('pre')).toBeNull();
    fireEvent.click(screen.getByText(zh?'技术详情':'Technical details'));
    expect((await screen.findByText(/SCM sample/)).textContent).toContain(raw);
  });
}

it('keeps recovery actions on the shell while the backend is unavailable', async () => {
  setLanguage('en');
  const call = vi.spyOn(transport, 'shell').mockResolvedValue('SCM state=1');
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, 'clipboard', {configurable: true, value: {writeText}});
  const { rerender } = render(<StartupPanel raw="startup_disabled: os error 2"/>);
  fireEvent.click(screen.getByText('Retry'));
  fireEvent.click(screen.getByText('Open Services'));
  fireEvent.click(screen.getByText('Open log folder'));
  fireEvent.click(screen.getByText('Copy diagnostic info'));
  expect(call).toHaveBeenCalledWith('startup_retry');
  expect(call).toHaveBeenCalledWith('startup_open_services');
  expect(call).toHaveBeenCalledWith('startup_open_logs');
  await vi.waitFor(() => expect(writeText).toHaveBeenCalledWith(expect.stringContaining('os error 2')));
  rerender(<StartupPanel raw="oink_update_in_progress"/>);
  fireEvent.click(screen.getByText('Download installer'));
  expect(call).toHaveBeenCalledWith('shell_open_url', {url:'https://github.com/oinkvm/oink/releases/latest'});
});
