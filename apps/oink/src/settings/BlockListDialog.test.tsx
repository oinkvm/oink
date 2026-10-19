// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { BlockListDialog } from './BlockListDialog';
import { setLanguage, tr } from '../i18n';
import { sectionsFor } from './schema';
import { signalLevel } from '../devices/icons';
import { fingerprint } from '../api/identity';
import { NearbyRow } from '../pairing/AddPanel';
beforeAll(() => {
  Object.defineProperty(window,'matchMedia',{value:()=>({matches:true,addEventListener:()=>{},removeEventListener:()=>{}})});
  vi.stubGlobal('ResizeObserver',class { observe(){} unobserve(){} disconnect(){} });
});
afterEach(cleanup);
const peer = {compatible:true,device:'1'.repeat(64),id:'1'.repeat(64),deviceId:'1234-5678-9ABC',name:'Device A',os:'win' as const,ip:'',port:0};
function show(language: 'en'|'zh-Hans', extras: Partial<Parameters<typeof BlockListDialog>[0]>={}) {
  setLanguage(language);
  return render(<OinkProvider locale={language==='en'?'en-US':'zh-CN'} defaultMotion="reduced"><BlockListDialog open blocked={[peer]} container={null} onClose={()=>{}} onRemove={()=>{}} {...extras}/></OinkProvider>);
}
describe('blocked-device management',()=>{
  it('shows identity hints and removes exactly the selected identity',async()=>{
    const remove=vi.fn();show('en',{onRemove:remove});
    expect(await screen.findByText('Device A')).toBeTruthy();
    expect(screen.getByText('1234-5678-9ABC')).toBeTruthy();
    fireEvent.click(screen.getByRole('button',{name:'Unblock Device A'}));
    expect(remove).toHaveBeenCalledWith(peer);
  });
  it('preserves the row while its removal is pending',async()=>{
    show('zh-Hans',{pending:[peer.id]});
    const button=await screen.findByRole('button',{name:'把 Device A 移出屏蔽名单'});
    expect((button as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText('Device A')).toBeTruthy();
  });
  it('distinguishes loading, empty and failed states in both languages',async()=>{
    show('en',{loading:true,blocked:[]});expect(await screen.findByText('Loading…')).toBeTruthy();
    expect(screen.queryByText('No blocked devices')).toBeNull();cleanup();
    show('en',{blocked:[]});expect(await screen.findByText('No blocked devices')).toBeTruthy();cleanup();
    show('zh-Hans',{error:'无法保存',blocked:[peer]});expect(await screen.findByRole('alert')).toBeTruthy();
    expect(screen.getByText('Device A')).toBeTruthy();
  });
});
describe('locale and unknown telemetry',()=>{
  it.each(['zh-Hans','en'] as const)('keeps incompatible nearby peers visible without pairing in %s',language=>{
    setLanguage(language);const connect=vi.fn();
    const problem=language==='en'?'Incompatible protocol version':'协议版本不兼容';
    render(<OinkProvider><NearbyRow peer={{...peer,compatible:false,problem}} onConnect={connect}/></OinkProvider>);
    expect(screen.getByText(problem)).toBeTruthy();
    expect((screen.getByRole('button') as HTMLButtonElement).disabled).toBe(true);
    expect(connect).not.toHaveBeenCalled();
  });
  it('re-evaluates settings metadata when the language changes',()=>{
    setLanguage('zh-Hans');expect(sectionsFor('mac')[0].title).toBe('通用');
    setLanguage('en');expect(sectionsFor('mac')[0].title).toBe('General');
    expect(tr('与 {0} 配对', 'Device A')).toBe('Pair with Device A');
    setLanguage('ja');expect(sectionsFor('mac')[0].title).toBe('一般');
    setLanguage('ko');expect(sectionsFor('mac')[0].title).toBe('일반');
  });
  it('uses unrounded RTT boundaries and the canonical 60-bit fingerprint',()=>{
    expect(signalLevel(19.99)).toBe(3);expect(signalLevel(20)).toBe(2);expect(signalLevel(50)).toBe(2);expect(signalLevel(50.01)).toBe(1);
    expect(fingerprint('0'.repeat(64))).toBe('0000-0000-0000');
    expect(fingerprint('f'.repeat(64))).toBe('ZZZZ-ZZZZ-ZZZZ');
  });
});
