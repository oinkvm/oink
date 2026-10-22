// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { setLanguage } from '../i18n';
import { SideContextMenu } from './DeviceMenu';
import type { SideContextMenuProps } from './DeviceMenu';
import type { Device } from './types';

beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
afterEach(() => { cleanup(); setLanguage('en'); });

const remote: Device = {
  id: 'remote', name: 'Remote', os: 'win', group: 'online', ip: '', port: 0, latency: 3, lastSeen: null,
  cursor: 'remote', screen: 'shared', screenSharing: true, mouse: 'shared', keyboard: 'shared', clipboard: 'shared', canDisableMouse: true,
  speaker: { mode: 'unused', volume: 50, volumeKnown: true, muted: false, canMakeExit: true }, gameMode: 'auto',
};
function menu(props: Partial<SideContextMenuProps> & { target: string }) {
  const noop = () => {};
  const handlers = { onShare: vi.fn(), onScreensHidden: vi.fn(), onHideScreen: vi.fn(), onScreenGain: vi.fn() };
  render(<OinkProvider defaultMotion="reduced"><SideContextMenu
    devices={[remote]} requests={[]} nearby={[]}
    onGameMode={noop} onPause={noop} onLeave={noop} onEditAddress={noop}
    onShowWindow={noop} onAbout={noop} onQuit={noop} onSpeaker={noop}
    onAccept={noop} onReject={noop} onPair={noop} onBlock={noop} {...handlers} {...props}
  ><button data-ctx={props.target}>Target</button></SideContextMenu></OinkProvider>);
  fireEvent.contextMenu(screen.getByRole('button', { name: 'Target' }));
  return handlers;
}
const items = async () => [...(await screen.findByRole('menu')).querySelectorAll('[role^="menuitem"]')].map(i => i.textContent);

it('draws device glyphs at menu icon size, like the prototype', async () => {
  menu({ target: 'device:remote' });
  for (const name of ['Input devices', 'Game mode', 'Set as audio output']) {
    const svg = (await screen.findByRole('menuitem', { name })).querySelector('svg');
    expect(svg?.parentElement?.classList.contains('devmenu__glyph'), name).toBe(true);
  }
});

it('lists the online device items in prototype order with a danger kick', async () => {
  menu({ target: 'device:remote', screens: [{ id: 'remote:1', device: 'remote', name: 'Display 1', hidden: false, gain: 1 }] });
  expect(await items()).toEqual(['Lock pointer', 'Hide all screens', 'Input devices', 'Game mode', 'Set as audio output', 'Pause sharing', 'Remove from group']);
  expect(screen.getByRole('menuitem', { name: 'Remove from group' }).className).toMatch(/danger/);
});

it('shows an open lock to unlock a locked pointer', async () => {
  menu({ target: 'device:remote', devices: [{ ...remote, cursor: 'locked' }] });
  const item = await screen.findByRole('menuitem', { name: 'Unlock pointer' });
  expect(item.querySelector('path')?.getAttribute('d')).toBe('M7.2 9V6.6a2.8 2.8 0 0 1 5.5-.7');
});

it('offers show all once any screen of the device is hidden', async () => {
  const handlers = menu({ target: 'device:remote', screens: [
    { id: 'remote:1', device: 'remote', name: 'Display 1', hidden: true, gain: 1 },
    { id: 'remote:2', device: 'remote', name: 'Display 2', hidden: false, gain: 1 },
  ] });
  expect(await items()).not.toContain('Show screen');
  fireEvent.click(screen.getByRole('menuitem', { name: 'Show all screens' }));
  expect(handlers.onScreensHidden).toHaveBeenCalledWith('remote', false);
});

it('hides all screens while none is hidden', async () => {
  const handlers = menu({ target: 'device:remote', screens: [{ id: 'remote:1', device: 'remote', name: 'Display 1', hidden: false, gain: 1 }] });
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Hide all screens' }));
  expect(handlers.onScreensHidden).toHaveBeenCalledWith('remote', true);
});

it('offers hiding and the pointer gain on a screen', async () => {
  const handlers = menu({ target: 'screen:remote:1', screens: [{ id: 'remote:1', device: 'remote', name: 'Display 1', hidden: false, gain: 1.25 }] });
  expect(await items()).toEqual(['Hide this screen']);
  expect(screen.getByRole('spinbutton', { name: 'Remote pointer speed on Display 1' })).toBeDefined(); expect((screen.getByRole('spinbutton', { name: 'Remote pointer speed on Display 1' }) as HTMLInputElement).value).toBe('1.25x');
  fireEvent.click(screen.getByRole('menuitem', { name: 'Hide this screen' }));
  expect(handlers.onHideScreen).toHaveBeenCalledWith('remote:1');
});

it('saves a burst of gain steps once, at 0.1 per press', async () => {
  vi.useFakeTimers();
  try {
    const handlers = menu({ target: 'screen:remote:1', screens: [{ id: 'remote:1', device: 'remote', name: 'Display 1', hidden: false, gain: 1 }] });
    await vi.runOnlyPendingTimersAsync();
    const input = screen.getByRole('spinbutton', { name: 'Remote pointer speed on Display 1' });
    fireEvent.keyDown(input, { key: 'ArrowUp' });
    fireEvent.keyDown(input, { key: 'ArrowUp' });
    expect(handlers.onScreenGain).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(300);
    expect(handlers.onScreenGain).toHaveBeenCalledTimes(1);
    expect(handlers.onScreenGain).toHaveBeenCalledWith('remote:1', 1.2);
  } finally { vi.useRealTimers(); }
});
