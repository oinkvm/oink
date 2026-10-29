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
  const handlers = { onShare: vi.fn(), onScreensHidden: vi.fn(), onHideScreen: vi.fn() };
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

it.each(['request', 'nearby'] as const)('blocks the selected %s without starting or rejecting a pair', async kind => {
  const peer = { id: kind === 'request' ? 'pair-id' : 'peer-id', device: 'peer-id',
    compatible: true, name: 'Peer', os: 'win' as const, ip: '', port: 47470, deviceId: 'fingerprint',
    revision: '1', sourceCount: 1, targetCount: 1 };
  const onBlock = vi.fn(), onPair = vi.fn(), onAccept = vi.fn(), onReject = vi.fn();
  menu({ target: `${kind}:${peer.id}`, requests: kind === 'request' ? [peer] : [],
    nearby: kind === 'nearby' ? [peer] : [], onBlock, onPair, onAccept, onReject });
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Block' }));
  expect(onBlock).toHaveBeenCalledExactlyOnceWith(peer);
  expect(onPair).not.toHaveBeenCalled();
  expect(onAccept).not.toHaveBeenCalled();
  expect(onReject).not.toHaveBeenCalled();
});

it('draws device glyphs at menu icon size, like the prototype', async () => {
  menu({ target: 'device:remote' });
  for (const name of ['Input devices', 'Game mode', 'Set as audio output']) {
    const svg = (await screen.findByRole('menuitem', { name })).querySelector('svg');
    expect(svg?.parentElement?.classList.contains('devmenu__glyph'), name).toBe(true);
  }
});

it('lists the online device items in prototype order with a danger kick', async () => {
  menu({ target: 'device:remote', screens: [{ id: 'remote:1', device: 'remote', name: 'Display 1', hidden: false }] });
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
    { id: 'remote:1', device: 'remote', name: 'Display 1', hidden: true },
    { id: 'remote:2', device: 'remote', name: 'Display 2', hidden: false },
  ] });
  expect(await items()).not.toContain('Show screen');
  fireEvent.click(screen.getByRole('menuitem', { name: 'Show all screens' }));
  expect(handlers.onScreensHidden).toHaveBeenCalledWith('remote', false);
});

it('hides all screens while none is hidden', async () => {
  const handlers = menu({ target: 'device:remote', screens: [{ id: 'remote:1', device: 'remote', name: 'Display 1', hidden: false }] });
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Hide all screens' }));
  expect(handlers.onScreensHidden).toHaveBeenCalledWith('remote', true);
});

it('offers only hiding on a screen', async () => {
  const handlers = menu({ target: 'screen:remote:1', screens: [{ id: 'remote:1', device: 'remote', name: 'Display 1', hidden: false }] });
  expect(await items()).toEqual(['Hide this screen']);
  expect(screen.queryByRole('spinbutton')).toBeNull();
  fireEvent.click(screen.getByRole('menuitem', { name: 'Hide this screen' }));
  expect(handlers.onHideScreen).toHaveBeenCalledWith('remote:1');
});


it('keeps application-focus settings and keyboard ownership out of device menus',async()=>{
  menu({target:'device:remote'});
  const labels=await items();
  expect(labels.join(' ')).not.toMatch(/Application focus|Enable on|Keyboard focus/);
});

it.each(['self','online'] as const)('permission actions stay on the owning device (%s)',async group=>{
  setLanguage('en');
  const grant=vi.fn();
  menu({target:'device:remote',devices:[{...remote,group,inputPermissionMissing:true,
    microphone:{mode:'idle',permissionGranted:false,virtualReady:true,volume:100,muted:false,canSetVolume:false}}],
    onGrantPermission:grant,onMicrophone:vi.fn()});
  const input=await screen.findByRole('menuitem',{name:'Input devices'});
  input.focus();
  fireEvent.keyDown(input,{key:'ArrowRight'});
  await screen.findByText('Microphone');
  if(group==='self') {
    expect(screen.getByRole('radio',{name:'Send'}).getAttribute('disabled')).not.toBeNull();
    expect(screen.getByRole('radio',{name:'Receive'}).getAttribute('disabled')).toBeNull();
    const inputGrant=screen.getAllByRole('button',{name:'Input Monitoring permission missing, grant access'});
    fireEvent.click(inputGrant[0]);
    expect(grant).toHaveBeenCalledWith('remote','input_monitoring');
  } else {
    expect(screen.queryByRole('button',{name:/permission missing, grant access/})).toBeNull();
    expect(grant).not.toHaveBeenCalled();
  }
});

it('keeps a failed source checked so it can stop forwarding', async () => {
  const source: Device = {...remote, speaker:{...remote.speaker, mode:'fault', configuredMode:'source'}, actionability:{speaker:'invoke'}};
  const exit: Device = {...remote, id:'exit', name:'Exit', speaker:{...remote.speaker, mode:'fault', configuredMode:'receiver'}};
  const handlers = menu({target:'device:remote', devices:[source,exit]});
  const item = await screen.findByRole('menuitemcheckbox', {name:'Forward audio to Exit'});
  expect(item.getAttribute('aria-checked')).toBe('true');
  fireEvent.click(item);
  expect(handlers.onShare).toHaveBeenCalledExactlyOnceWith('remote','speaker','unused');
});

it('can stop a failed source even while no exit is online', async () => {
  const source: Device = {...remote, speaker:{...remote.speaker, mode:'fault', configuredMode:'source'}, actionability:{speaker:'invoke'}};
  const handlers=menu({target:'device:remote',devices:[source]});
  fireEvent.click(await screen.findByRole('menuitem',{name:'Stop sharing'}));
  expect(handlers.onShare).toHaveBeenCalledExactlyOnceWith('remote','speaker','unused');
});
