import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { readFileSync } from 'node:fs';

const request = vi.hoisted(() => vi.fn());
vi.mock('../api/transport', () => ({ transport: { shell: request } }));
const decode = vi.fn();
let dimensions = [750, 750];
let sponsorQr: typeof import('./remoteQr').sponsorQr;

beforeEach(async () => {
  vi.resetModules(); request.mockReset(); decode.mockReset().mockResolvedValue(undefined);
  dimensions = [750, 750];
  vi.stubGlobal('Image', class {
    src = '';
    naturalWidth = dimensions[0]; naturalHeight = dimensions[1];
    decode = decode;
  });
  ({ sponsorQr } = await import('./remoteQr'));
});
afterEach(() => { vi.unstubAllGlobals(); vi.useRealTimers(); });

it.each(['image/png', 'image/jpeg'])('uses decoded %s data without another network request', async mime_type => {
  request.mockResolvedValue({ mime_type, base64: 'UE5H' });
  expect(await sponsorQr()).toBe(`data:${mime_type};base64,UE5H`);
  await sponsorQr();
  expect(request).toHaveBeenCalledExactlyOnceWith('shell_sponsor_qr');
  expect(decode).toHaveBeenCalledOnce();
});

it.each(['failure', 'missing', 'non-image', 'too large', 'invalid base64', 'empty', 'corrupt'])('falls back after %s without retrying', async reason => {
  request.mockResolvedValue({ mime_type: 'image/png', base64: 'UE5H' });
  if (reason === 'failure') request.mockRejectedValue(new Error('offline'));
  if (reason === 'missing') request.mockResolvedValue(null);
  if (reason === 'non-image') request.mockResolvedValue({ mime_type: 'text/html', base64: 'UE5H' });
  if (reason === 'too large') request.mockResolvedValue({ mime_type: 'image/png', base64: Buffer.alloc(2 * 1024 * 1024 + 1).toString('base64') });
  if (reason === 'invalid base64') request.mockResolvedValue({ mime_type: 'image/png', base64: '!not base64!' });
  if (reason === 'empty') request.mockResolvedValue({ mime_type: 'image/png', base64: '' });
  if (reason === 'corrupt') decode.mockRejectedValue(new Error('decode failed'));
  expect(await sponsorQr()).toBeNull();
  expect(await sponsorQr()).toBeNull();
  expect(request).toHaveBeenCalledOnce();
  if (!['failure', 'missing', 'corrupt'].includes(reason)) expect(decode).not.toHaveBeenCalled();
});

it.each([[0, 750], [63, 750], [750, 2049]])('rejects unreasonable dimensions %s × %s', async (width, height) => {
  dimensions = [width, height];
  request.mockResolvedValue({ mime_type: 'image/png', base64: 'UE5H' });
  expect(await sponsorQr()).toBeNull();
});

it('bounds a stalled decoder', async () => {
  vi.useFakeTimers();
  request.mockResolvedValue({ mime_type: 'image/png', base64: 'UE5H' });
  decode.mockReturnValue(new Promise(() => {}));
  const pending = sponsorQr();
  await vi.advanceTimersByTimeAsync(2000);
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
