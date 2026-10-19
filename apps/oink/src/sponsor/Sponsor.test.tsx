// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { WeChatQrDialog } from './Sponsor';
import { sponsorQr } from './remoteQr';
import bundled from './wechat-qr.png';
const css = readFileSync('src/sponsor/sponsor.css', 'utf8');

vi.mock('./remoteQr', () => ({ sponsorQr: vi.fn() }));
vi.mock('../api/client', () => ({ shell: {} }));
vi.mock('../api/store', () => ({ agent: {} }));
vi.mock('oink-ui', () => ({ Dialog: ({ children }: { children: React.ReactNode }) => <div>{children}</div>, Button: () => null }));

beforeEach(() => { vi.mocked(sponsorQr).mockReset(); });
afterEach(cleanup);

it('renders the bundle immediately while the startup download is pending', async () => {
  let complete!: (src: string | null) => void;
  vi.mocked(sponsorQr).mockReturnValue(new Promise(resolve => { complete = resolve; }));
  render(<WeChatQrDialog open onOpenChange={() => {}} container={null} />);
  const image = screen.getByRole('img');
  const alt = image.getAttribute('alt');
  expect(image.getAttribute('src')).toBe(bundled);
  expect(image.getAttribute('width')).toBe('220');
  expect(image.getAttribute('height')).toBe('220');
  await act(async () => { complete('data:image/png;base64,UE5H'); });
  expect(image.getAttribute('src')).toBe('data:image/png;base64,UE5H');
  expect(image.getAttribute('alt')).toBe(alt);
  expect(image.getAttribute('width')).toBe('220');
  expect(image.getAttribute('height')).toBe('220');
  fireEvent.error(image);
  expect(image.getAttribute('src')).toBe(bundled);
});

it('keeps the bundled QR on failure and its white 220px box in dark mode', async () => {
  vi.mocked(sponsorQr).mockResolvedValue(null);
  const style = document.createElement('style'); style.textContent = css; document.head.append(style);
  render(<div data-theme="dark"><WeChatQrDialog open onOpenChange={() => {}} container={null} /></div>);
  await act(async () => {});
  const image = screen.getByRole('img');
  expect(image.getAttribute('src')).toBe(bundled);
  expect(getComputedStyle(image).backgroundColor).toBe('rgb(255, 255, 255)');
  expect(getComputedStyle(image).width).toBe('220px');
  expect(getComputedStyle(image).height).toBe('220px');
  style.remove();
});
