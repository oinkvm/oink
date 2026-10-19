import { afterEach, expect, it, vi } from 'vitest';
import { api, shell } from './client';
import { agent, start, stop } from './store';

vi.mock('./client', () => ({
  ApiFailure: class extends Error {},
  api: Object.fromEntries(['launchAtLoginStatus', 'diagnosticsStatus', 'updateStatus', 'deviceCards', 'blockedDevices', 'overview', 'layout', 'networkInfo', 'inputSettings', 'clipboardSettings', 'audioStatus', 'permissions', 'fileProviderStatus', 'wallpapers'].map(key => [key, vi.fn().mockResolvedValue(null)])),
  shell: { bootInfo: vi.fn().mockResolvedValue({ platform: 'macos', backend: 'agent' }) },
  call: vi.fn().mockResolvedValue(null),
  onAgentEvent: vi.fn().mockResolvedValue(() => {}),
}));
vi.mock('./pairing', () => ({ pairingApi: { nearby: vi.fn().mockResolvedValue(null), pairings: vi.fn().mockResolvedValue(null) } }));

afterEach(() => { stop(); vi.restoreAllMocks(); });

it('mounts the resolved onboarding route while optional wallpaper loading is pending', async () => {
  let resolveOverview!: (value: Awaited<ReturnType<typeof api.overview>>) => void;
  let resolveWallpaper!: (value: Awaited<ReturnType<typeof api.wallpapers>>) => void;
  vi.mocked(api.overview).mockImplementation(() => new Promise(resolve => { resolveOverview = resolve; }));
  vi.mocked(api.wallpapers).mockImplementation(() => new Promise(resolve => { resolveWallpaper = resolve; }));
  let ready = false;
  const startup = start().then(() => { ready = true; });
  await vi.waitFor(() => expect(api.wallpapers).toHaveBeenCalled());
  expect(ready).toBe(false);
  resolveOverview({ onboarding_done: false } as Awaited<ReturnType<typeof api.overview>>);
  await vi.waitFor(() => expect(ready).toBe(true), { timeout: 100 });
  expect(agent.get().overview?.onboarding_done).toBe(false);
  expect(agent.get().boot?.platform).toBe('macos');
  expect(shell.bootInfo).toHaveBeenCalledOnce();
  resolveWallpaper([]);
  await startup;
});
