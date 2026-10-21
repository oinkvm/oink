// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { useState } from 'react';
import { OinkProvider } from 'oink-ui';
import { UpdateCheck, UpdatePreferences } from './UpdatePreferences';
import type { UpdateStatus } from '../api/update';
import { setLanguage } from '../i18n';
const mock = vi.hoisted(()=>({update:vi.fn(),open:vi.fn(),openUrl:vi.fn(),fail:vi.fn(),query:vi.fn(),openDialog:vi.fn()}));
vi.mock('../api/client',()=>({api:{update:mock.update},shell:{open:mock.open,openUrl:mock.openUrl}}));
vi.mock('../api/store',()=>({agent:{query:mock.query,fail:mock.fail}}));
vi.mock('../window/Upgrade',()=>({openUpdateDialog:mock.openDialog}));
const status:UpdateStatus = {current_build:30,version:'0.1.1',checks_enabled:true,
  phase:'idle',candidate:null,checked_at:null,next_check:null,deferred_until:null,blockers:[],progress:null,error:null,scope:'user',can_change_policy:true};
beforeAll(()=>{
  Object.defineProperty(window,'matchMedia',{value:()=>({matches:true,addEventListener(){},removeEventListener(){}})});
  vi.stubGlobal('ResizeObserver',class{observe(){}unobserve(){}disconnect(){}});
});
beforeEach(()=>{vi.clearAllMocks();setLanguage('en');mock.query.mockImplementation((_key:string,action:()=>unknown)=>action());mock.update.mockResolvedValue(status);});
afterEach(cleanup);
const show=(s:UpdateStatus)=>render(<OinkProvider defaultMotion="reduced"><UpdatePreferences status={s}/></OinkProvider>);

// Mirrors the agent store: the check reply becomes the current status, and
// backend events replace it later. `open` is the Settings page's own state.
let publish:(next:UpdateStatus)=>void=()=>{};
let setOpen:(open:boolean)=>void=()=>{};
function Harness({initial}:{initial:UpdateStatus}) {
  const [current,setCurrent]=useState(initial);publish=setCurrent;
  const [open,changeOpen]=useState(true);setOpen=changeOpen;
  return <UpdateCheck status={current} open={open}/>;
}
const check=(initial:UpdateStatus)=>{
  mock.query.mockImplementation(async(_key:string,action:()=>Promise<UpdateStatus>)=>{publish(await action());});
  return render(<OinkProvider defaultMotion="reduced"><Harness initial={initial}/></OinkProvider>);
};
const button=()=>screen.getByRole('button',{name:'Check for updates'});
const result=()=>document.querySelector('[data-update-result]')?.textContent??'';
const checked={...status,checked_at:1_800_000_000};
const available={...checked,phase:'available' as const,candidate:{build:31,version:'0.1.2',size:1,published_at:null,severity:'normal',notes:{en:'Fixes','zh-Hans':'修复'}}};
// Click and wait for the reply: the backend has moved to Checking.
const start=async()=>{
  mock.update.mockResolvedValue({...status,phase:'checking'});
  fireEvent.click(button());
  await waitFor(()=>expect(mock.update).toHaveBeenCalledExactlyOnceWith({type:'check'}));
};

it('shows no text before a check, even when the backend checked earlier',()=>{
  check(checked);
  expect(result()).toBe('');
  expect(button().getAttribute('aria-busy')).toBeNull();
});
it('the button loads and is disabled while checking, with no inline text',async()=>{
  check(status);await start();
  expect(button().getAttribute('aria-busy')).toBe('true');
  expect(button()).toHaveProperty('disabled',true);
  expect(result()).toBe('');
  expect(document.body.textContent).not.toContain('Checking');
});
it('an up-to-date check shows Oink is up to date without a dialog',async()=>{
  check(status);await start();
  act(()=>publish(checked));
  expect(result()).toBe('Oink is up to date');
  expect(button().getAttribute('aria-busy')).toBeNull();
  expect(button()).toHaveProperty('disabled',false);
  expect(mock.openDialog).not.toHaveBeenCalled();
  expect(mock.open).not.toHaveBeenCalled();
});
it('the result is cleared when Settings closes or remounts',async()=>{
  const view=check(status);await start();
  act(()=>publish(checked));
  expect(result()).toBe('Oink is up to date');
  act(()=>setOpen(false));
  expect(result()).toBe('');
  act(()=>setOpen(true));
  expect(result()).toBe('');
  mock.update.mockClear();await start();act(()=>publish(checked));
  expect(result()).toBe('Oink is up to date');
  view.unmount();
  check(checked);
  expect(result()).toBe('');
});
it('the result is cleared when the window is hidden',async()=>{
  check(status);await start();act(()=>publish(checked));
  Object.defineProperty(document,'hidden',{configurable:true,get:()=>true});
  act(()=>{document.dispatchEvent(new Event('visibilitychange'));});
  Object.defineProperty(document,'hidden',{configurable:true,get:()=>false});
  expect(result()).toBe('');
});
it('opens the update dialog only when the check finds an update',async()=>{
  check(status);await start();
  expect(mock.openDialog).not.toHaveBeenCalled();
  act(()=>publish(available));
  expect(mock.openDialog).toHaveBeenCalledOnce();
  expect(result()).toBe('');
  expect(button()).toHaveProperty('disabled',false);
});
it('an update that is already pending opens the dialog from the reply',async()=>{
  check(available);
  mock.update.mockResolvedValue(available);
  fireEvent.click(button());
  await waitFor(()=>expect(mock.openDialog).toHaveBeenCalledOnce());
});
it('shows a failed check inline without a dialog',async()=>{
  check(status);await start();
  act(()=>publish({...status,phase:'failed',error:'network'}));
  expect(result()).toBe('Update failed');
  expect(mock.openDialog).not.toHaveBeenCalled();
});
it('shows a rejected check command inline',async()=>{
  check(status);
  mock.update.mockRejectedValue(new Error('network'));
  fireEvent.click(button());
  await waitFor(()=>expect(result()).toBe('Update failed'));
  expect(button()).toHaveProperty('disabled',false);
  expect(mock.openDialog).not.toHaveBeenCalled();
});
it('keeps a manual download route for disabled versions',async()=>{
  show({...status,phase:'disabled'});
  expect(screen.getByText('Download and install updates manually for this version.')).toBeTruthy();
  expect(screen.queryByRole('switch')).toBeNull();
  cleanup();
  mock.openUrl.mockResolvedValue(undefined);
  check({...status,phase:'disabled'});
  fireEvent.click(button());
  await waitFor(()=>expect(mock.openUrl).toHaveBeenCalledWith('https://github.com/oinkvm/oink/releases'));
  expect(mock.update).not.toHaveBeenCalled();
  expect(result()).toBe('');
});
it('saves the exact persisted policy through typed IPC without optimistic changes',async()=>{
  show(status);
  fireEvent.click(screen.getByRole('switch',{name:'Automatically check for updates'}));
  await waitFor(()=>expect(mock.update).toHaveBeenCalledWith({type:'set_preferences',preferences:{checks_enabled:false}}));
  expect(screen.getByRole('switch')).toHaveProperty('checked',true);
});
it('disables unauthorized machine policy changes',()=>{
  show({...status,scope:'machine',can_change_policy:false});
  expect(screen.getByRole('switch')).toHaveProperty('disabled',true);
  fireEvent.click(screen.getByRole('switch'));
  expect(mock.update).not.toHaveBeenCalled();
});
it('shows only the automatic-check switch row',()=>{
  show(status);
  expect(screen.getByRole('switch',{name:'Automatically check for updates'})).toBeTruthy();
  expect(screen.queryByRole('combobox')).toBeNull();
  expect(screen.queryByText('Install automatically when idle')).toBeNull();
  expect(document.querySelector('.setp__update')?.textContent).toBe('Automatically check for updates ');
  expect(document.querySelector('.setp__update')?.children).toHaveLength(1);
});
