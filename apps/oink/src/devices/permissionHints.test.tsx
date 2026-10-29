// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { PermissionHint } from '../permissions/PermissionHint';
import { DeviceItem } from './DeviceList';
import { SideContextMenu, type SideContextMenuProps } from './DeviceMenu';
import type { Device } from './types';
import { setLanguage } from '../i18n';
vi.mock('../permissions/PermissionHint', async original => {
  const actual = await original<typeof import('../permissions/PermissionHint')>();
  return { ...actual, PermissionHint: vi.fn(actual.PermissionHint) };
});
const base: Device = { id: 'device', name: 'Device', group: 'self', os: 'mac', ip: '', port: 0, latency: 1, lastSeen: null,
  cursor: 'local', screen: 'shared', mouse: 'shared', keyboard: 'shared', clipboard: 'shared', gameMode: null, speaker: { mode: 'unused', volume: 100, muted: false },
  microphone: { mode: 'idle', volume: 100, muted: false, permissionGranted: true, virtualReady: true, canSetVolume: false } };
beforeEach(() => {
  setLanguage('en'); vi.clearAllMocks();
  Object.defineProperty(window, 'matchMedia', { configurable: true, value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
afterEach(cleanup);
const cases = [
  { kind: 'accessibility', patch: { accessibilityMissing: true }, target: '.dev__accessibility' },
  { kind: 'input_monitoring', patch: { inputPermissionMissing: true }, target: '.dev-ico' },
  { kind: 'input_monitoring', patch: { inputPermissionMissing: true }, target: '.dev-ico:nth-child(2)' },
  { kind: 'permissions', patch: { actionability: { mouse: 'open_permissions' } }, target: '.dev-ico' },
  { kind: 'permissions', patch: { actionability: { keyboard: 'open_permissions' } }, target: '.dev-ico:nth-child(2)' },
  { kind: 'permissions', patch: { mouse: 'fault' }, target: '.dev-ico' },
] satisfies { kind: string; patch: Partial<Device>; target: string }[];
it.each(cases.flatMap(c => [true, false].filter(local => local || c.patch.mouse !== 'fault').map(local => ({ ...c, local }))))('device row $kind local=$local uses the shared hint for its icon and action', async ({ kind, patch, target, local }) => {
  const grant = vi.fn(), toggle = vi.fn(), microphone = vi.fn();
  const { container } = render(<OinkProvider defaultMotion="reduced"><DeviceItem d={{ ...base, ...patch, group: local ? 'self' : 'online' }} now={0} selected={false} collapsed={false}
    onSelect={() => {}} onGrantAccessibility={grant} onGrantPermission={grant} onToggle={toggle} onMicrophone={microphone}/></OinkProvider>);
  expect(vi.mocked(PermissionHint).mock.calls.some(([props]) => props.kind === kind && props.local === local)).toBe(true);
  const icon = container.querySelector<HTMLElement>(target)!;
  fireEvent.pointerEnter(icon, { pointerType: 'mouse' });
  const tooltip = await screen.findByRole('tooltip');
  const link = tooltip.querySelector('button');
  if (local) {
    expect(icon.tagName).toBe('BUTTON');
    expect(link).not.toBeNull();
    fireEvent.click(link!);
    fireEvent.click(icon, { detail: 0 });
    const action = kind === 'virtual_microphone' ? microphone : kind === 'permissions' ? toggle : grant;
    expect(action).toHaveBeenCalledTimes(2);
  } else {
    expect(icon.tagName).toBe('SPAN');
    expect(link).toBeNull();
    fireEvent.click(icon);
    expect(grant).not.toHaveBeenCalled(); expect(toggle).not.toHaveBeenCalled(); expect(microphone).not.toHaveBeenCalled();
  }
});

it.each([true, false])('input-device menus use the shared hint for both permissions, local=%s', async local => {
  const grant = vi.fn(), noop = () => {};
  const props = { devices: [{ ...base, group: local ? 'self' : 'online', inputPermissionMissing: true, microphone: { ...base.microphone!, permissionGranted: false } }],
    requests: [], nearby: [], onGrantPermission: grant, onShare: noop, onHideScreen: noop, onScreensHidden: noop, onGameMode: noop, onPause: noop, onLeave: noop,
    onEditAddress: noop, onShowWindow: noop, onAbout: noop, onQuit: noop, onSpeaker: noop, onAccept: noop, onReject: noop, onPair: noop, onBlock: noop } satisfies Omit<SideContextMenuProps, 'children'>;
  render(<OinkProvider defaultMotion="reduced"><SideContextMenu {...props}><button data-ctx="device:device">Target</button></SideContextMenu></OinkProvider>);
  fireEvent.contextMenu(screen.getByText('Target'));
  const inputs = await screen.findByRole('menuitem', { name: 'Input devices' });
  inputs.focus(); fireEvent.keyDown(inputs, { key: 'ArrowRight' });
  await screen.findByText('Microphone');
  for (const [index, kind] of ['input_monitoring', 'input_monitoring'].entries()) {
    expect(vi.mocked(PermissionHint).mock.calls.some(([props]) => props.kind === kind && props.local === local)).toBe(true);
    const control = document.querySelectorAll('.devmenu__permission')[index];
    fireEvent.pointerEnter(control, { pointerType: 'mouse' });
    const tooltip = await screen.findByRole('tooltip');
    if (local) {
      fireEvent.click(tooltip.querySelector('button')!);
      expect(grant).toHaveBeenLastCalledWith('device', kind);
    } else expect(tooltip.querySelector('button')).toBeNull();
    fireEvent.pointerLeave(control, { pointerType: 'mouse' });
    await waitFor(() => expect(screen.queryByRole('tooltip')).toBeNull());
  }
});

it.each([true,false])('microphone requirements stay in action lines and local links only (%s)',async local=>{
  const grant=vi.fn(),mic=vi.fn();
  const {container}=render(<OinkProvider><DeviceItem d={{...base,group:local?'self':'online',microphone:{...base.microphone!,mode:'muted',permissionGranted:false,virtualReady:false}}} now={0} selected={false} collapsed={false} onSelect={()=>{}} onMicrophone={mic} onGrantPermission={grant}/></OinkProvider>);
  const icon=container.querySelectorAll('.dev-ico')[3];
  fireEvent.pointerEnter(icon,{pointerType:'mouse'});
  const tip=await screen.findByRole('tooltip');
  const lines=tip.querySelectorAll('.dev-ico__tip > span');
  expect(lines).toHaveLength(3);
  expect(lines[0].textContent).toBe('Microphone: Local');
  expect(tip.querySelectorAll('button')).toHaveLength(local?2:0);
  if(local){
    fireEvent.click(screen.getByRole('button',{name:'Installation required'}));
    expect(mic).toHaveBeenCalledWith('install');
    fireEvent.click(screen.getByRole('button',{name:'Permission required'}));
    expect(grant).toHaveBeenCalledWith('microphone');
  } else expect(tip.textContent).toBe('Microphone: LocalClick: Receive a microphone shared by another deviceDouble-click: Share this device’s microphone');
});

it('uses approved Chinese copy and a comma before each microphone requirement link',async()=>{
  setLanguage('zh-Hans');
  const install=vi.fn();
  const {container}=render(<OinkProvider><DeviceItem d={{...base,microphone:{...base.microphone!,mode:'muted',virtualReady:false,permissionGranted:false}}} now={0} selected={false} collapsed={false} onSelect={()=>{}} onMicrophone={install} onGrantPermission={()=>{}}/></OinkProvider>);
  fireEvent.pointerEnter(container.querySelectorAll('.dev-ico')[3],{pointerType:'mouse'});
  const tip=await screen.findByRole('tooltip');
  expect([...tip.querySelectorAll('.dev-ico__tip > span')].map(line=>line.textContent)).toEqual(['麦克风：本地','单击：接收其他设备共享的麦克风，需要安装','双击：共享本机麦克风，需要授权']);
  fireEvent.click(screen.getByRole('button',{name:'需要安装'}));
  expect(install).toHaveBeenCalledWith('install');
});

it('a sending microphone offers no setup link after stopping sharing',async()=>{
  setLanguage('zh-Hans');
  const {container}=render(<OinkProvider><DeviceItem d={{...base,microphone:{...base.microphone!,mode:'source',captureActive:true,virtualReady:false,permissionGranted:true}}} now={0} selected={false} collapsed={false} onSelect={()=>{}} onMicrophone={()=>{}} onGrantPermission={()=>{}}/></OinkProvider>);
  fireEvent.pointerEnter(container.querySelectorAll('.dev-ico')[3],{pointerType:'mouse'});
  const tip=await screen.findByRole('tooltip');
  expect([...tip.querySelectorAll('.dev-ico__tip > span')].map(line=>line.textContent)).toEqual(['麦克风：发送','单击：静音','双击：停止共享']);
});
