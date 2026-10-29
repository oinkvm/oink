// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { PermissionHint, type PermissionHintKind } from './PermissionHint';
import { setLanguage } from '../i18n';

const kinds: PermissionHintKind[] = ['accessibility', 'input_monitoring', 'microphone', 'virtual_microphone', 'permissions'];
beforeEach(() => {
  vi.useFakeTimers();
  Object.defineProperty(window, 'matchMedia', { configurable: true, value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
afterEach(() => { cleanup(); vi.useRealTimers(); setLanguage('en'); });

it.each((['en', 'zh-Hans', 'ja', 'ko'] as const).flatMap(language => kinds.map(kind => ({ language, kind }))))('$language $kind keeps the local action open on hover and invokes the same grant as the trigger', async ({ language, kind }) => {
  setLanguage(language);
  const grant = vi.fn();
  const { container } = render(<OinkProvider><PermissionHint kind={kind} local onGrant={grant}>{({ label, onGrant }) => <button aria-label={label} onClick={onGrant}>Indicator</button>}</PermissionHint></OinkProvider>);
  const trigger = container.querySelector('button')!;
  fireEvent.pointerEnter(trigger, { pointerType: 'mouse' });
  await act(async () => { await vi.advanceTimersByTimeAsync(500); });
  const tooltip = screen.getByRole('tooltip');
  expect(tooltip.getAttribute('data-interactive')).not.toBe('false');
  fireEvent.pointerLeave(trigger, { pointerType: 'mouse' });
  await act(async () => { await vi.advanceTimersByTimeAsync(50); });
  fireEvent.pointerEnter(tooltip, { pointerType: 'mouse' });
  await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
  expect(tooltip.getAttribute('data-state')).toBe('open');
  const link = within(tooltip).getByRole('button');
  expect(link.textContent).toBeTruthy();
  expect(tooltip.textContent).not.toMatch(/\{0\}|undefined/);
  fireEvent.click(link);
  expect(grant).toHaveBeenCalledTimes(1);
  fireEvent.click(trigger, { detail: 0 }); // native Enter/Space activation
  expect(grant).toHaveBeenCalledTimes(2);
});

it.each(kinds)('%s never offers a remote action, even if a callback was supplied', async kind => {
  setLanguage('en');
  const grant = vi.fn();
  const { container } = render(<OinkProvider><PermissionHint kind={kind} local={false} onGrant={grant}>{({ label, onGrant }) => <span role="img" aria-label={label} onClick={onGrant}>Indicator</span>}</PermissionHint></OinkProvider>);
  const trigger = container.querySelector('span')!;
  fireEvent.pointerEnter(trigger, { pointerType: 'mouse' });
  await act(async () => { await vi.advanceTimersByTimeAsync(500); });
  const tooltip = screen.getByRole('tooltip');
  expect(tooltip.getAttribute('data-interactive')).toBe('false');
  expect(tooltip.textContent).not.toMatch(/on that device|grant access|install/i);
  expect(within(tooltip).queryByRole('button')).toBeNull();
  expect(within(tooltip).queryByRole('link')).toBeNull();
  fireEvent.click(trigger);
  expect(grant).not.toHaveBeenCalled();
});
