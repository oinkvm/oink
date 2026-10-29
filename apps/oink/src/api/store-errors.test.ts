import { afterEach, expect, it, vi } from 'vitest';
import { toast } from 'oink-ui';
import { api, ApiFailure } from './client';
import { createAgentStore } from './store';
import { problemText } from '../i18n';
import { captureNotices } from './notices.fixture';
const notices = captureNotices();

const unavailable = () => new ApiFailure({code:'unavailable',problem:{code:'local_service_unavailable',unknownCode:null,nativeCode:null,retryAfterMs:null}});
afterEach(() => vi.restoreAllMocks());

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
