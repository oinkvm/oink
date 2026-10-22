// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import type { ReactNode } from 'react';
import { PromoArea } from '../window/PromoArea';
import { sponsorList } from './remoteList';
import { fallbackItems, localized, type SponsorItem } from './list';
import { setLanguage } from '../i18n';

const openUrl = vi.hoisted(() => vi.fn().mockResolvedValue(undefined));
vi.mock('./remoteList', () => ({ sponsorList: vi.fn() }));
vi.mock('../api/client', () => ({ shell: { openUrl } }));
vi.mock('../api/store', () => ({ agent: { fail: vi.fn() } }));
vi.mock('oink-ui', () => ({
  Dialog: ({ title, children, actions }: { title: string; children: ReactNode; actions: ReactNode }) => <div role="dialog" aria-label={title}>{children}{actions}</div>,
  Button: ({ children, onClick, 'aria-label': label }: { children: ReactNode; onClick?: () => void; 'aria-label'?: string }) => <button onClick={onClick} aria-label={label}>{children}</button>,
  DropdownMenu: ({ children, trigger }: { children: ReactNode; trigger: ReactNode }) => <div>{trigger}<div role="menu">{children}</div></div>,
  MenuItem: ({ icon, children, onSelect }: { icon: ReactNode; children: ReactNode; onSelect: () => void }) => <button role="menuitem" onClick={onSelect}>{icon}{children}</button>,
  Tooltip: ({ children }: { children: ReactNode }) => children,
}));
const remote: SponsorItem[] = [
  { id: 'link', name: { default: 'Cloud link', locales: { zh: '云端链接', ja: 'リンク' } }, icon: 'data:image/png;base64,aWNvbg==', action: { type: 'link', url: 'https://example.org/support' } },
  { id: 'qr', name: { default: 'Cloud QR', locales: { zh: '云端扫码' } }, icon: 'data:image/png;base64,aWNvbg==', action: { type: 'qr', image: 'data:image/png;base64,cXI=', caption: { default: 'Scan to support', locales: { zh: '扫码支持' } } } },
];
beforeEach(() => { vi.mocked(sponsorList).mockReset(); openUrl.mockClear(); setLanguage('en'); });
afterEach(cleanup);

it('shows the complete built-in list while pending, then replaces it in cloud order', async () => {
  let complete!: (items: SponsorItem[] | null) => void;
  vi.mocked(sponsorList).mockReturnValue(new Promise(resolve => { complete = resolve; }));
  render(<PromoArea />);
  expect(screen.getAllByRole('menuitem').map(node => node.textContent)).toEqual(['Ko-fi', 'WeChat Pay']);
  fireEvent.click(screen.getByRole('menuitem', { name: 'Ko-fi' }));
  expect(openUrl).toHaveBeenCalledWith(fallbackItems[0].action.type === 'link' && fallbackItems[0].action.url);
  fireEvent.click(screen.getByRole('menuitem', { name: 'WeChat Pay' }));
  expect(screen.getByRole('dialog').getAttribute('aria-label')).toBe('WeChat Pay');
  expect(screen.getByRole('img').getAttribute('src')).toBe(fallbackItems[1].action.type === 'qr' && fallbackItems[1].action.image);
  fireEvent.click(screen.getByText('Close'));
  await act(async () => complete(remote));
  expect(screen.getAllByRole('menuitem').map(node => node.textContent)).toEqual(['Cloud link', 'Cloud QR']);
  const icon = screen.getAllByRole('menuitem')[0].querySelector('img')!;
  expect(icon.getAttribute('src')).toBe(remote[0].icon);
  expect(icon.getAttribute('width')).toBe('20');
  fireEvent.click(screen.getByRole('menuitem', { name: 'Cloud link' }));
  expect(openUrl).toHaveBeenLastCalledWith('https://example.org/support');
  fireEvent.click(screen.getByRole('menuitem', { name: 'Cloud QR' }));
  expect(screen.getByRole('dialog').getAttribute('aria-label')).toBe('Cloud QR');
  expect(screen.getByRole('img').getAttribute('src')).toBe('data:image/png;base64,cXI=');
  expect(screen.getByText('Scan to support')).toBeTruthy();
});

it.each([null, []])('keeps the fallback for an unavailable or empty remote list: %s', async items => {
  vi.mocked(sponsorList).mockResolvedValue(items);
  render(<PromoArea />); await act(async () => {});
  expect(screen.getAllByRole('menuitem').map(node => node.textContent)).toEqual(['Ko-fi', 'WeChat Pay']);
});

it('updates names and dialog captions immediately on locale changes and falls back to default', async () => {
  vi.mocked(sponsorList).mockResolvedValue(remote);
  render(<PromoArea />); await act(async () => {});
  act(() => setLanguage('zh-Hans'));
  fireEvent.click(screen.getByRole('menuitem', { name: '云端扫码' }));
  expect(screen.getByRole('dialog').getAttribute('aria-label')).toBe('云端扫码');
  expect(screen.getByText('扫码支持')).toBeTruthy();
  act(() => setLanguage('ja'));
  expect(screen.getByRole('menuitem', { name: 'リンク' })).toBeTruthy();
  expect(screen.getByRole('dialog').getAttribute('aria-label')).toBe('Cloud QR');
  expect(screen.getByText('Scan to support')).toBeTruthy();
  expect(localized({ default: 'Default', locales: { en: 'English', ko: '한국어' } }, 'en')).toBe('English');
  expect(localized({ default: 'Default', locales: { ko: '한국어' } }, 'ko')).toBe('한국어');
});

it('returns to a usable fallback on a late image error and retains the white QR box', async () => {
  vi.mocked(sponsorList).mockResolvedValue(remote);
  const style = document.createElement('style');
  style.textContent = readFileSync('src/sponsor/sponsor.css', 'utf8'); document.head.append(style);
  render(<div data-theme="dark"><PromoArea /></div>); await act(async () => {});
  fireEvent.click(screen.getByRole('menuitem', { name: 'Cloud QR' }));
  fireEvent.error(screen.getByRole('img'));
  expect(screen.queryByRole('dialog')).toBeNull();
  expect(screen.getByRole('menuitem', { name: 'Ko-fi' })).toBeTruthy();
  fireEvent.click(screen.getByRole('menuitem', { name: 'WeChat Pay' }));
  const image = screen.getByRole('img');
  expect(getComputedStyle(image).backgroundColor).toBe('rgb(255, 255, 255)');
  expect(getComputedStyle(image).width).toBe('220px');
  expect(getComputedStyle(image).height).toBe('220px');
  style.remove();
});

it('preserves the hide-sponsor preference', async () => {
  vi.mocked(sponsorList).mockResolvedValue(remote);
  render(<PromoArea sponsor={false} />); await act(async () => {});
  expect(screen.queryByRole('menu')).toBeNull();
  expect(screen.getByRole('button', { name: 'GitHub' })).toBeTruthy();
});
