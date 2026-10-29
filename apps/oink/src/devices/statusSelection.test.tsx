// @vitest-environment jsdom
import { useState } from 'react';
import { cleanup, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeAll, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { DeviceList } from './DeviceList';
import type { Device } from './types';

beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', {value: () => ({matches:false, addEventListener(){}, removeEventListener(){}})});
});
afterEach(cleanup);
const device: Device = {
  id:'fixture', name:'Device', os:'mac', group:'self', ip:'', port:0, latency:null,
  lastSeen:null, cursor:'local', screen:'shared', mouse:'shared', keyboard:'shared',
  clipboard:'shared', gameMode:'auto', canDisableMouse:true,
  speaker:{mode:'exit', volume:80, volumeKnown:true, mutedKnown:true, canSetVolume:true, muted:false},
  microphone:{mode:'source', permissionGranted:true, virtualReady:true, volume:100, muted:false, canSetVolume:true},
};
function setup(d = device, selected = false) {
  const action=vi.fn();
  function Harness() {
    const [id,setId]=useState<string|null>(selected ? d.id : null);
    const [shown,setShown]=useState(d);
    const send=(...args:unknown[])=>{action(...args);setShown({...d});};
    return <OinkProvider defaultMotion="reduced"><DeviceList devices={[shown]} selectedId={id} onSelect={setId}
      onToggle={send} onSpeaker={send} onMicrophone={send} onGrantPermission={send} collapsed={false}/></OinkProvider>;
  }
  return {...render(<Harness/>),action};
}
it.each(['mouse','keyboard','speaker','microphone'])('selects on %s click and performs its action immediately', kind => {
  const {container,action}=setup();
  const icon=container.querySelectorAll('.dev-ico')[['mouse','keyboard','speaker','microphone'].indexOf(kind)];
  expect(container.querySelector('[data-current]')).toBeNull();
  fireEvent.click(icon,{detail:1});
  expect(container.querySelector('[data-current]')).not.toBeNull();
  expect(container.querySelector('.dev__hit')?.getAttribute('aria-current')).toBe('true');
  expect(action).toHaveBeenCalledExactlyOnceWith('fixture', ...(kind==='speaker' || kind==='microphone' ? [{type:'mute'}] : [kind,'single']));
});
it.each(['mouse','keyboard','speaker','microphone'])('preserves immediate %s double click and Shift+Enter', kind => {
  const {container,action}=setup();
  const icon=container.querySelectorAll('.dev-ico')[['mouse','keyboard','speaker','microphone'].indexOf(kind)];
  fireEvent.click(icon,{detail:1});
  expect(action).toHaveBeenCalledTimes(1);
  fireEvent.click(icon,{detail:2});
  expect(action.mock.calls[1]).toEqual(kind==='microphone'?['fixture','local']:['fixture',kind,'double']);
  cleanup();
  const second=setup();
  fireEvent.keyDown(second.container.querySelectorAll('.dev-ico')[['mouse','keyboard','speaker','microphone'].indexOf(kind)],{key:'Enter',shiftKey:true});
  expect(second.container.querySelector('[data-current]')).not.toBeNull();
  expect(second.action).toHaveBeenCalledTimes(1);
});
it.each([0,1,3])('selects when status icon %i opens a permission action', index => {
  const {container,action}=setup({...device,inputPermissionMissing:true,microphone:{...device.microphone!,permissionGranted:false}});
  fireEvent.click(container.querySelectorAll('.dev-ico')[index]);
  expect(container.querySelector('[data-current]')).not.toBeNull();
  expect(action).toHaveBeenCalledTimes(1);
});
it('selects through a status icon without an available action', () => {
  const {container,action}=setup({...device,unknown:true,mouse:'unknown'});
  fireEvent.click(container.querySelector('.dev-ico')!);
  expect(container.querySelector('[data-current]')).not.toBeNull();
  expect(action).not.toHaveBeenCalled();
});
it.each([0,1,3])('keeps the selected speaker control mounted and enabled on unrelated icon %i', index => {
  const {container}=setup(device,true);
  const button=container.querySelector<HTMLButtonElement>('.vol__btn')!;
  const glyph=button.querySelector('.vol__glyph');
  fireEvent.click(container.querySelectorAll('.dev-ico')[index]);
  expect(container.querySelector('.vol__btn')).toBe(button);
  expect(button.querySelector('.vol__glyph')).toBe(glyph);
  expect(button.disabled).toBe(false);
  expect(container.querySelector('[data-current]')).not.toBeNull();
});

it.each(['mouse','keyboard','speaker','microphone'])('keeps an already selected device selected on %s click', kind => {
  const action=vi.fn();
  function Harness() {
    const [id,setId]=useState<string|null>(device.id);
    return <OinkProvider defaultMotion="reduced"><DeviceList devices={[device]} selectedId={id} onSelect={x=>setId(p=>p===x?null:x)}
      onToggle={action} onSpeaker={action} onMicrophone={action} onGrantPermission={action} collapsed={false}/></OinkProvider>;
  }
  const {container}=render(<Harness/>);
  const icon=container.querySelectorAll('.dev-ico')[['mouse','keyboard','speaker','microphone'].indexOf(kind)];
  fireEvent.click(icon,{detail:1});
  expect(container.querySelector('.dev__hit')?.getAttribute('aria-current')).toBe('true');
  expect(action).toHaveBeenCalledOnce();
});
