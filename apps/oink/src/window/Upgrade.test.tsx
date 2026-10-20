// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { UpgradeButton } from './Upgrade';
import type { UpdateStatus } from '../api/update';
import { setLanguage } from '../i18n';
const mock=vi.hoisted(()=>({update:vi.fn(),query:vi.fn(),fail:vi.fn(),status:null as UpdateStatus|null}));
vi.mock('../api/client',()=>({api:{update:mock.update},shell:{close:vi.fn(),openUrl:vi.fn()}}));
vi.mock('../api/store',()=>({agent:{query:mock.query,fail:mock.fail},useAgent:()=>({update:mock.status})}));
beforeEach(()=>{
  setLanguage('en');vi.clearAllMocks();
  Object.defineProperty(window,'matchMedia',{configurable:true,value:()=>({matches:true,addEventListener(){},removeEventListener(){}})});
  vi.stubGlobal('ResizeObserver',class{observe(){}unobserve(){}disconnect(){}});
  mock.status={current_build:30,version:'0.1.0',channel:'stable',checks_enabled:true,phase:'ready',candidate:{build:31,version:'0.1.1',size:1,published_at:null,severity:'normal',notes:{en:'Fixes','zh-Hans':'修复'}},checked_at:null,next_check:null,deferred_until:null,blockers:['audio','remote_input','locked'],progress:100,error:null,scope:'user',can_change_policy:true};
  mock.query.mockImplementation((_key:string,action:()=>unknown)=>action());mock.update.mockResolvedValue({...mock.status,phase:'quiescing',blockers:[]});
});
afterEach(cleanup);
it('clicking Install submits once immediately despite active-sharing observations',async()=>{
  render(<OinkProvider defaultMotion="reduced"><UpgradeButton container={null} standalone/></OinkProvider>);
  expect(screen.getByText('Ready to install')).toBeTruthy();
  expect(screen.getByText('Installation immediately interrupts input, audio and file sharing, then restarts Oink.')).toBeTruthy();
  fireEvent.click(screen.getByRole('button',{name:'Restart to update'}));
  await waitFor(()=>expect(mock.update).toHaveBeenCalledExactlyOnceWith({type:'install',expected_build:31}));
  expect(screen.queryByText('Waiting for idle')).toBeNull();
});
