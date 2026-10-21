// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { App } from '../App';
import { api } from '../api/client';
import { agent } from '../api/store';
import type { Overview } from '../api/types';
import { setLanguage } from '../i18n';

vi.mock('oink-ui', async original => ({ ...await original<typeof import('oink-ui')>(), speed: () => 0 }));
vi.mock('../api/store', async original => ({ ...await original<typeof import('../api/store')>(), start: vi.fn() }));
vi.mock('../api/transport', async original => ({
  ...await original<typeof import('../api/transport')>(),
  transport: { call: async ({ method }: { method: string }) => method === 'group' ? { members: [] } : [], shell: async () => null, onEvent: async () => () => {} },
}));

beforeAll(() => {
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  Element.prototype.animate = vi.fn(() => ({ cancel() {}, finished: Promise.resolve() })) as unknown as typeof Element.prototype.animate;
  HTMLElement.prototype.scrollTo = () => {};
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
let enable: ReturnType<typeof vi.spyOn>;
beforeEach(() => {
  setLanguage('en');
  enable = vi.spyOn(api, 'openFileProviderEnable').mockResolvedValue(null);
  agent.adopt('boot', { platform: 'macos', version: '0.1.0', build: null, backend: 'agent' });
  agent.adopt('fileProvider', { state: 'not_enabled', needed: true, opening: false, detail: null } as never);
});
afterEach(() => { cleanup(); agent.reset(); localStorage.clear(); vi.restoreAllMocks(); });

const TEXT = 'In the Finder window that opens, click Enable in the top-right corner.';
async function answer(button: 'OK' | 'Cancel', before = 0) {
  const dialog = await screen.findByRole('dialog', { name: 'Enable File Provider' });
  expect(within(dialog).getByText(TEXT)).toBeTruthy();
  expect(enable).toHaveBeenCalledTimes(before);
  await act(async () => { fireEvent.click(within(dialog).getByRole('button', { name: button })); });
  await waitFor(() => expect(screen.queryByRole('dialog', { name: 'Enable File Provider' })).toBeNull());
}

describe('File Provider explainer', () => {
  const settingsButton = async (container: HTMLElement) => {
    fireEvent.click(container.querySelector('[data-action="settings"]')!);
    const row = await waitFor(() => container.querySelector<HTMLElement>('[data-item="perm-files"]')!);
    // `hidden`: jsdom still reports the settings page as hidden while it slides in.
    return within(row).getByRole('button', { name: 'Enable', hidden: true });
  };

  it.each([['OK', 1], ['Cancel', 0]] as const)('asks before Settings opens Finder; %s', async (choice, calls) => {
    const { container } = render(<App/>);
    fireEvent.click(await settingsButton(container));
    await answer(choice);
    expect(enable).toHaveBeenCalledTimes(calls);
    // Asking again works the same way (the second click is not a different action).
    fireEvent.click(await settingsButton(container));
    await answer('OK', calls);
    expect(enable).toHaveBeenCalledTimes(calls + 1);
  });

  it.each([['OK', 1], ['Cancel', 0]] as const)('asks once before onboarding opens Finder; %s', async (choice, calls) => {
    agent.adopt('overview', { this_device: { id: 'self', fingerprint: '0000' }, alerts: [], onboarding_done: false,
      pause: { paused: false, scope: 'local', revision: 1, author: null, author_name: null, emergency: false, capture_detached: false, settling: false } } as unknown as Overview);
    render(<App/>);
    fireEvent.click(await screen.findByRole('button', { name: 'Next' }));
    const row = (await screen.findByText('File Provider')).closest<HTMLElement>('.ok-row')!;
    fireEvent.click(within(row).getByRole('button', { name: 'Enable' }));
    expect(await screen.findAllByRole('dialog', { name: 'Enable File Provider' })).toHaveLength(1);
    await answer(choice);
    expect(enable).toHaveBeenCalledTimes(calls);
  });
});
