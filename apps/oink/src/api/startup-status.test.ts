import { expect, it, vi } from 'vitest';
import { waitForStartup } from './store';

const mocks = vi.hoisted(() => ({
  listen: vi.fn(), shell: vi.fn(), stop: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => ({listen: mocks.listen}));
vi.mock('./transport', async original => ({
  ...await original<typeof import('./transport')>(),
  inTauri: true, transport: {shell: mocks.shell},
}));

it('keeps a newer startup notification over an old snapshot and finishes on ready', async () => {
  let notify!: (event: {payload: string}) => void;
  mocks.listen.mockImplementation(async (_, callback) => { notify = callback; return mocks.stop; });
  mocks.shell.mockImplementation(async () => {
    notify({payload:'startup_stopped: 1067'});
    return 'waiting: os error 2';
  });
  const show = vi.fn();
  const task = waitForStartup(show);
  await vi.waitFor(() => expect(show).toHaveBeenCalledWith('startup_stopped: 1067'));
  notify({payload:'starting'});
  notify({payload:'ready'});
  await task;
  expect(show).toHaveBeenCalledTimes(1);
  expect(mocks.stop).toHaveBeenCalledOnce();
});
