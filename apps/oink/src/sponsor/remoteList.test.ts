import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import type { SponsorItem } from './list';
const request = vi.hoisted(() => vi.fn());
vi.mock('../api/transport', () => ({ transport: { shell: request } }));
const decode = vi.fn();
let sponsorList: typeof import('./remoteList').sponsorList;
function items(): SponsorItem[] { return [
  { id: 'link', name: { default: 'Link' }, icon: 'data:image/png;base64,UE5H', action: { type: 'link', url: 'https://example.org' } },
  { id: 'qr', name: { default: 'QR' }, icon: 'data:image/jpeg;base64,UE5H', action: { type: 'qr', image: 'data:image/png;base64,UE5H' } },
]; }
beforeEach(async () => {
  vi.resetModules(); request.mockReset(); decode.mockReset().mockResolvedValue(undefined);
  vi.stubGlobal('Image', class { src = ''; decode = decode; });
  ({ sponsorList } = await import('./remoteList'));
});
afterEach(() => { vi.unstubAllGlobals(); vi.useRealTimers(); });
it('decodes both action types locally, caching one shell command per WebView', async () => {
  request.mockResolvedValue(items());
  expect(await sponsorList()).toEqual(items());
  await sponsorList();
  expect(request).toHaveBeenCalledExactlyOnceWith('shell_sponsor_list');
  expect(decode).toHaveBeenCalledTimes(3);
});
it.each(['offline', 'missing', 'empty', 'too many', 'corrupt'])('keeps fallback for %s without retrying', async reason => {
  request.mockResolvedValue(items());
  if (reason === 'offline') request.mockRejectedValue(new Error('offline'));
  if (reason === 'missing') request.mockResolvedValue(null);
  if (reason === 'empty') request.mockResolvedValue([]);
  if (reason === 'too many') request.mockResolvedValue(Array(9).fill(items()[0]));
  if (reason === 'corrupt') decode.mockRejectedValue(new Error('corrupt'));
  expect(await sponsorList()).toBeNull(); expect(await sponsorList()).toBeNull();
  expect(request).toHaveBeenCalledOnce();
});
it.each(['remote URL', 'SVG', 'invalid base64', 'oversize'])('rejects unsafe image payloads: %s', async reason => {
  const src = ({ 'remote URL': 'https://example.org/icon.png', SVG: 'data:image/svg+xml;base64,UE5H', 'invalid base64': 'data:image/png;base64,!bad!', oversize: 'data:image/png;base64,' + 'A'.repeat(2_800_000) })[reason]!;
  const list = items(); list[0].icon = src; list[1].action = { type: 'qr', image: src };
  request.mockResolvedValue(list); expect(await sponsorList()).toBeNull();
});
it('drops individual browser decoding failures', async () => {
  request.mockResolvedValue(items()); decode.mockRejectedValueOnce(new Error('corrupt icon'));
  expect((await sponsorList())?.map(item => item.id)).toEqual(['qr']);
});
it('rejects non-HTTPS link targets', async () => {
  const list = items(); list[0].action = { type: 'link', url: 'javascript:alert(1)' };
  request.mockResolvedValue(list); expect((await sponsorList())?.map(item => item.id)).toEqual(['qr']);
});
it('bounds a stalled browser decoder', async () => {
  vi.useFakeTimers(); request.mockResolvedValue(items()); decode.mockReturnValue(new Promise(() => {}));
  const pending = sponsorList(); await vi.advanceTimersByTimeAsync(2000);
  expect(await pending).toBeNull();
});
it('keeps the WebView CSP and capabilities closed to remote images and connections', () => {
  const config = JSON.parse(readFileSync(new URL('../../src-tauri/tauri.conf.json', import.meta.url), 'utf8'));
  const directives = Object.fromEntries(config.app.security.csp.split(';').map((value: string) => {
    const [name, ...sources] = value.trim().split(/\s+/); return [name, sources];
  }));
  expect(directives['img-src']).toEqual(["'self'", 'data:']);
  expect(directives['connect-src']).toEqual(['ipc:', 'http://ipc.localhost']);
  const capabilities = JSON.parse(readFileSync(new URL('../../src-tauri/capabilities/default.json', import.meta.url), 'utf8'));
  expect(capabilities.remote).toBeUndefined();
  expect(capabilities.permissions.some((p: string) => p.startsWith('http:'))).toBe(false);
});
