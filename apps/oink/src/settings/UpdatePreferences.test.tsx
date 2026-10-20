// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { UpdatePreferences, checkForUpdates } from './UpdatePreferences';
import type { UpdateStatus } from '../api/update';
import { setLanguage } from '../i18n';
const mock = vi.hoisted(()=>({update:vi.fn(),open:vi.fn(),openUrl:vi.fn(),fail:vi.fn(),query:vi.fn()}));
vi.mock('../api/client',()=>({api:{update:mock.update},shell:{open:mock.open,openUrl:mock.openUrl}}));
vi.mock('../api/store',()=>({agent:{query:mock.query,fail:mock.fail}}));
const status:UpdateStatus = {current_build:30,version:'0.1.1',channel:'stable',checks_enabled:true,
  phase:'idle',candidate:null,checked_at:null,next_check:null,deferred_until:null,blockers:[],progress:null,error:null,scope:'user',can_change_policy:true};
beforeAll(()=>{
  Object.defineProperty(window,'matchMedia',{value:()=>({matches:true,addEventListener(){},removeEventListener(){}})});
  vi.stubGlobal('ResizeObserver',class{observe(){}unobserve(){}disconnect(){}});
});
beforeEach(()=>{vi.clearAllMocks();setLanguage('en');mock.query.mockImplementation((_key:string,action:()=>unknown)=>action());mock.update.mockResolvedValue(status);});
afterEach(cleanup);
const show=(s:UpdateStatus)=>render(<OinkProvider defaultMotion="reduced"><UpdatePreferences status={s}/></OinkProvider>);

it('uses native check and opens update details only after acknowledgement',async()=>{
  await checkForUpdates(status);
  expect(mock.update).toHaveBeenCalledWith({type:'check'});
  expect(mock.open).toHaveBeenCalledWith('upgrade');
  expect(mock.openUrl).not.toHaveBeenCalled();
  mock.open.mockClear();mock.update.mockRejectedValueOnce(new Error('network'));
  await expect(checkForUpdates(status)).rejects.toThrow('network');
  expect(mock.open).not.toHaveBeenCalled();
});
it('keeps a manual download route for disabled versions',async()=>{
  show({...status,phase:'disabled'});
  expect(screen.getByText('Download and install updates manually for this version.')).toBeTruthy();
  expect(screen.queryByRole('switch')).toBeNull();
  await checkForUpdates({...status,phase:'disabled'});
  expect(mock.update).not.toHaveBeenCalled();
  expect(mock.openUrl).toHaveBeenCalledWith('https://github.com/oinkvm/oink/releases');
});
it('saves the exact persisted policy through typed IPC without optimistic changes',async()=>{
  show(status);
  fireEvent.click(screen.getByRole('switch',{name:'Automatically check for updates'}));
  await waitFor(()=>expect(mock.update).toHaveBeenCalledWith({type:'set_preferences',preferences:{checks_enabled:false,channel:'stable'}}));
  expect(screen.getByRole('switch')).toHaveProperty('checked',true);
  expect(screen.getByText('Update settings apply to the current user.')).toBeTruthy();
});
it('explains machine policy and disables unauthorized changes',()=>{
  show({...status,scope:'machine',can_change_policy:false});
  expect(screen.getByText(/Changes require administrator permission/)).toBeTruthy();
  expect(screen.getByRole('switch')).toHaveProperty('disabled',true);
  for(const field of screen.getAllByRole('combobox'))expect(field).toHaveProperty('disabled',true);
  fireEvent.click(screen.getByRole('switch'));
  expect(mock.update).not.toHaveBeenCalled();
});
it('has automatic checks but no automatic-install setting or idle promise',()=>{
  show(status);
  expect(screen.getByRole('switch',{name:'Automatically check for updates'})).toBeTruthy();
  expect(screen.getAllByRole('combobox')).toHaveLength(1);
  expect(screen.queryByText('Install automatically when idle')).toBeNull();
  expect(screen.getByText('After downloading, click Install to interrupt sharing and restart Oink immediately.')).toBeTruthy();
});
