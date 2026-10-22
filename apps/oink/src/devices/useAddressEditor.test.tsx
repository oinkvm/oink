// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, renderHook, screen } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
const {memberAddresses,setMemberAddress}=vi.hoisted(()=>({memberAddresses:vi.fn(),setMemberAddress:vi.fn()}));
vi.mock('../api/client',()=>({api:{memberAddresses,setMemberAddress}}));
import { ApiFailure, toFailure } from '../api/transport';
import { useAddressEditor } from './useAddressEditor';
import { AddressDialog } from './AddressDialog';
import { setLanguage } from '../i18n';
import type { Device } from './types';
const device:Device={id:'remote',name:'Remote',os:'mac',group:'offline',ip:'',port:47470,latency:null,lastSeen:null,cursor:'unknown',screen:'unknown',mouse:'unknown',keyboard:'unknown',clipboard:'unknown',speaker:{mode:'unknown',volume:0,volumeKnown:false,muted:false},gameMode:null,unknown:true};
beforeAll(()=>{
  Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})});
  vi.stubGlobal('ResizeObserver',class {observe(){} unobserve(){} disconnect(){}});
});
beforeEach(()=>{vi.resetAllMocks();setLanguage('en');});
afterEach(cleanup);
describe('offline address editing',()=>{
  it('reads and saves retained addresses without requiring the peer to be online',async()=>{
    memberAddresses.mockResolvedValue(['192.0.2.4:47470','192.0.2.5:47470']);
    setMemberAddress.mockResolvedValue(null);
    const {result}=renderHook(useAddressEditor);
    await act(()=>result.current.open(device));
    expect(result.current.device).toMatchObject({group:'offline',ip:'192.0.2.4',port:47470});
    expect(result.current.addresses).toHaveLength(2);
    expect(result.current.problem).toBeNull();expect(result.current.pending).toBe(false);
    render(<OinkProvider defaultMotion="reduced"><AddressDialog {...result.current} container={null} onCancel={result.current.cancel} onConfirm={(d,ip,port)=>{void result.current.save(d,ip,port);}}/></OinkProvider>);
    expect(await screen.findByText(/Device is offline/)).toBeTruthy();
    expect(screen.getByText(/Saved addresses/).textContent).toContain('192.0.2.5:47470');
    // No placeholder text in the IP box.
    expect(screen.getByLabelText('IP address').hasAttribute('placeholder')).toBe(false);
    fireEvent.change(screen.getByLabelText('IP address'),{target:{value:'192.0.2.9'}});
    await act(async()=>{fireEvent.click(screen.getByRole('button',{name:'Confirm'}));});
    expect(setMemberAddress).toHaveBeenCalledWith('remote','192.0.2.9:47470');
    expect(result.current.device).toBeNull();
  });
  it.each([
    ['invalid_address','Enter a valid IPv4 address and port'],
    ['not_member','Device is not in this group'],
    ['persistence_failed','Could not save settings'],
    ['network_stopped','Network service stopped'],
    ['local_service_unavailable','Cannot reach the Oink backend'],
  ] as const)('preserves %s on load and save and keeps editing available',async(code,message)=>{
    const failure=new ApiFailure({code:code==='local_service_unavailable'?'unavailable':code,problem:{code,unknownCode:null,nativeCode:null,retryAfterMs:null}});
    memberAddresses.mockRejectedValue(failure);setMemberAddress.mockRejectedValue(failure);
    const {result}=renderHook(useAddressEditor);
    await act(()=>result.current.open(device));
    expect(result.current.problem).toBe(message);expect(result.current.device).not.toBeNull();expect(result.current.pending).toBe(false);
    await act(()=>result.current.save(device,'192.0.2.4',47470));
    expect(result.current.problem).toBe(message);expect(result.current.device).not.toBeNull();expect(result.current.pending).toBe(false);
  });
  it('does not label native decoding or command errors as an IPC disconnect',()=>{
    expect(toFailure(new Error('unknown command')).problem.code).toBe('backend_failed');
  });
  it('ignores a late address reply after opening another device',async()=>{
    let finish!:(addresses:string[])=>void;
    memberAddresses.mockImplementationOnce(()=>new Promise(resolve=>{finish=resolve;})).mockResolvedValueOnce(['192.0.2.5:47470']);
    const {result}=renderHook(useAddressEditor);
    let first!:Promise<void>;act(()=>{first=result.current.open(device);});
    await act(()=>result.current.open({...device,id:'other'}));
    await act(async()=>{finish(['192.0.2.4:47470']);await first;});
    expect(result.current.device).toMatchObject({id:'other',ip:'192.0.2.5'});
  });
});
