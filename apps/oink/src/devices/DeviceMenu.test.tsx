// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { setLanguage, tr } from '../i18n';
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

it.each([
  ['zh-Hans', '禁用该设备的鼠标（防误触）', '禁用该设备的键盘（防误触）'],
  ['en', 'Disable this device’s mouse (prevents accidental input)', 'Disable this device’s keyboard (prevents accidental input)'],
  ['ja', 'このデバイスのマウスを無効にする（誤操作防止）', 'このデバイスのキーボードを無効にします（誤操作防止）'],
  ['ko', '이 기기의 마우스 비활성화 (오조작 방지)', '이 기기의 키보드를 비활성화합니다 (오조작 방지)'],
] as const)('%s explains why mouse and keyboard can be disabled', async (language, mouse, keyboard) => {
  setLanguage(language);
  menu({ target: 'device:remote' });
  const input = await screen.findByRole('menuitem', { name: tr('输入设备') });
  input.focus();
  fireEvent.keyDown(input, { key: 'ArrowRight' });
  await screen.findByRole('radio', { name: `${tr('禁用')}：${mouse}` });
  const labels = [...document.querySelectorAll('[aria-label]')].map(el => el.getAttribute('aria-label'));
  expect(labels.some(label => label?.endsWith(mouse))).toBe(true);
  expect(labels.some(label => label?.endsWith(keyboard))).toBe(true);
});

it('offers whole-device hide for offline canvas screens and device rows', async () => {
  const handlers = menu({ target: 'screen:remote:1', devices: [{ ...remote, group: 'offline' }],
    screens: [{ id: 'remote:1', device: 'remote', name: 'One', hidden: false }] });
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Hide all screens' }));
  expect(handlers.onScreensHidden).toHaveBeenCalledWith('remote', true);
  expect(handlers.onHideScreen).not.toHaveBeenCalled();
});
it('allows offline device rows to show hidden screens', async () => {
  const handlers = menu({ target: 'device:remote', devices: [{ ...remote, group: 'offline', screenSharing: false }] });
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Show all screens' }));
  expect(handlers.onScreensHidden).toHaveBeenCalledWith('remote', false);
});

it('offers single-screen hide for an online rigid computer', async () => {
  const handlers = menu({ target: 'screen:remote:1', editableScreenDevices: new Set(),
    screens: [{ id: 'remote:1', device: 'remote', name: 'One', hidden: false }] });
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Hide this screen' }));
  expect(handlers.onHideScreen).toHaveBeenCalledWith('remote:1');
  expect(handlers.onScreensHidden).not.toHaveBeenCalled();
});
