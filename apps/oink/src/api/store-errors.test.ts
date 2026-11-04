import { afterEach, expect, it, vi } from 'vitest';
import { toast } from 'oink-ui';
import { api, ApiFailure, shell } from './client';
import { backendFailureDetail, createAgentStore, reportBackendFailure, start, stop } from './store';
import { transport } from './transport';
import { currentLanguage, problemText, setLanguage, tr } from '../i18n';
import { captureNotices } from './notices.fixture';
const notices = captureNotices();
const initialLanguage = currentLanguage();

const unavailable = () => new ApiFailure({code:'unavailable',problem:{code:'local_service_unavailable',unknownCode:null,nativeCode:null,retryAfterMs:null}});
afterEach(() => { stop(); vi.restoreAllMocks(); vi.unstubAllGlobals(); setLanguage(initialLanguage); });

it('does not retain a failed IPC request as the current backend health', () => {
  const notify = vi.spyOn(toast, 'danger').mockReturnValue('notice');
  const store = createAgentStore();
  store.fail('local_service_unavailable');
  store.receive({type:'permissions',data:[]});
  expect(notices()).toEqual([]);
  expect(notify).toHaveBeenCalledWith(expect.objectContaining({title:problemText('local_service_unavailable')}));
});

it('does not label an unclassified shell or rendering error as lost IPC', () => {
  const notify = vi.spyOn(toast, 'danger').mockReturnValue('notice');
  const store = createAgentStore();
  store.fail('unavailable');
  expect(notices()).toEqual([]);
  expect(notify).toHaveBeenCalledWith(expect.objectContaining({title:problemText('backend_failed')}));
});

it.each(['legacy', 'app action', 'startup'] as const)('preserves, translates and copies the backend exit detail through %s', async path => {
  const notify = vi.spyOn(toast, 'danger').mockReturnValue('notice');
  const writeText=vi.fn().mockResolvedValue(undefined);
  vi.stubGlobal('navigator',{clipboard:{writeText}});
  const error = {code:'unavailable',message:'backend_exited:3221225477',problem:{code:'local_service_unavailable',unknownCode:null,nativeCode:null,retryAfterMs:null}} as const;
  const failure = new ApiFailure(error);
  // Native IPC rejects with a serialized object, not an ApiFailure instance.
  vi.spyOn(transport,'call').mockRejectedValue(error);
  vi.spyOn(transport,'onEvent').mockResolvedValue(() => {});
  vi.spyOn(shell,'bootInfo').mockResolvedValue({backend:'none',platform:'windows',version:'0.3.2',build:null,open_update_failure:false});
  const store = createAgentStore();
  for (const language of ['zh-Hans','en']) {
    setLanguage(language);
    notify.mockClear();
    if (path === 'legacy') store.fail(failure.problem.code,failure.detail);
    else if (path === 'app action') await api.completeOnboarding().catch(reportBackendFailure);
    else await start();
    expect(notify).toHaveBeenCalled();
    const options=notify.mock.calls.at(-1)![0]!;
    if (typeof options === 'string') throw new Error('expected detailed toast');
    expect(options.title).toBe(problemText('local_service_unavailable'));
    expect(options.duration).toBe(0);
    expect(options.description).toContain('3221225477 / 0xC0000005');
    expect(options.description).toContain(language==='en'?'backend exited after launch':'后台启动后退出');
    expect(options.description).toBe(backendFailureDetail(failure.detail!));
    expect(options.action!.label).toBe(tr('复制错误详情'));
    await options.action!.onClick!();
    expect(writeText).toHaveBeenLastCalledWith(options.description);
    expect(notices()).toEqual([]);
    stop();
  }
});

it.each(['unsupported', 'permission_denied', 'backend_failed', 'volume_unsupported', 'outcome_unknown'] as const)('keeps the existing app failure behavior for %s', async code => {
  const notify = vi.spyOn(toast, 'danger').mockReturnValue('notice');
  vi.spyOn(transport,'call').mockRejectedValue({code:'internal',message:'must not appear in a toast',problem:{code,unknownCode:null,nativeCode:null,retryAfterMs:null}});
  await api.completeOnboarding().catch(reportBackendFailure);
  expect(notify).not.toHaveBeenCalled();
  expect(notices()).toEqual(code==='volume_unsupported'||code==='outcome_unknown'?[]:[code]);
});

it('keeps an unclassified app failure generic and a typed IPC failure specific', () => {
  const notify = vi.spyOn(toast, 'danger').mockReturnValue('notice');
  reportBackendFailure(new Error('unclassified rendering failure'));
  expect(notify).toHaveBeenLastCalledWith({id:'request:backend_failed',title:problemText('backend_failed'),duration:4200});
  reportBackendFailure(unavailable());
  expect(notify).toHaveBeenLastCalledWith({id:'request:local_service_unavailable',title:problemText('local_service_unavailable'),duration:4200});
  expect(notices()).toEqual([]);
});

it.each(['new snapshot', 'retired session'])('ignores a late failed snapshot after %s', async reason => {
  const store = createAgentStore();
  let reject!: (error:unknown) => void;
  const pending = store.refresh('permissions', () => new Promise((_, fail) => { reject = fail; }));
  if(reason === 'new snapshot') store.receive({type:'permissions',data:[]});
  else store.reset();
  reject(unavailable());
  await expect(pending).resolves.toBeUndefined();
  expect(notices()).toEqual([]);
});

it('still reports a current snapshot failure to its caller', async () => {
  const store = createAgentStore();
  const failure = unavailable();
  await expect(store.query('permissions', () => Promise.reject(failure))).rejects.toBe(failure);
});

it('preserves a wallpaper error without inventing a backend outage', async () => {
  const notify = vi.spyOn(toast, 'danger').mockReturnValue('notice');
  vi.spyOn(api, 'wallpapers').mockRejectedValue(new ApiFailure({code:'unsupported',problem:{code:'unsupported',unknownCode:null,nativeCode:null,retryAfterMs:null}}));
  const store = createAgentStore();
  store.receive({type:'wallpapers'});
  // Drain the query and its rejection handler, without a timer or real backend.
  await Promise.resolve(); await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
  expect(notices()).toEqual([]);
  expect(notify).toHaveBeenCalledWith(expect.objectContaining({title:problemText('unsupported')}));
});

it('does not discard a command failure just because a newer snapshot arrived', async () => {
  const store = createAgentStore();
  let reject!: (error:unknown) => void;
  const pending = store.query('permissions', () => new Promise((_, fail) => { reject = fail; }));
  store.receive({type:'permissions',data:[]});
  const failure = unavailable(); reject(failure);
  await expect(pending).rejects.toBe(failure);
});

it.each([
  {type:'dnd_export_failed',data:'failed'},
  {type:'dnd_export_failed',data:'timeout'},
  {type:'dnd_preparation_failed',data:'prepared metadata or identity changed'},
] as const)('shows no notice for a drag failure: $type $data', event => {
  const notify = vi.spyOn(toast, 'danger').mockReturnValue('notice');
  const store = createAgentStore();
  store.receive(event);
  expect(notices()).toEqual([]);
  expect(notify).not.toHaveBeenCalled();
});
