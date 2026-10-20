// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { setLanguage } from '../i18n';
import { DeviceItem } from './DeviceList';
import { SideContextMenu } from './DeviceMenu';
import type { Device } from './types';
beforeAll(()=>{
  Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})});
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); setLanguage('en'); });
const device:Device={
  id:'remote',name:'Remote',os:'win',group:'online',ip:'',port:0,latency:null,lastSeen:null,
  cursor:'unknown',screen:'unknown',mouse:'unknown',keyboard:'unknown',clipboard:'unknown',
  speaker:{mode:'unknown',volume:0,volumeKnown:false,muted:false},gameMode:null,unknown:true,
};
describe('device-owned game badges',()=>{
  it('renders every active owner regardless of focus and clears owner updates',()=>{
    const rows=(active:boolean[]) => <OinkProvider>{active.map((gameActive,index)=><DeviceItem
      key={index} d={{...device,id:String(index),name:`Device ${index}`,group:index===0?'self':'online',
        cursor:index===0?'local':'remote',screen:'shared',gameMode:index===0?'on':'auto',gameActive}}
      now={0} selected={false} collapsed={false} onSelect={()=>{}}/>)}</OinkProvider>;
    const {container,rerender}=render(rows([true,true]));
    expect(container.querySelectorAll('.os-game')).toHaveLength(2);
    rerender(rows([true,false]));
    expect(container.querySelectorAll('.os-game')).toHaveLength(1);
    expect(container.querySelector('[data-ctx="device:1"] .os-game')).toBeNull();
    rerender(rows([false,false]));
    expect(container.querySelectorAll('.os-game')).toHaveLength(0);
  });
});
describe('unavailable device row',()=>{
  it('allows an explicit exit selection while historical ordering is unknown',()=>{
    setLanguage('en');
    const toggle=vi.fn();
    const d={...device,unknown:false,speaker:{...device.speaker,canMakeExit:true}};
    render(<OinkProvider><DeviceItem d={d} now={0} selected={false} collapsed={false} onSelect={()=>{}} onToggle={toggle}/></OinkProvider>);
    const output=screen.getByRole('button',{name:/Double-click to set as group output/});
    fireEvent.click(output,{detail:1});
    expect(toggle).not.toHaveBeenCalled();
    fireEvent.click(output,{detail:2});
    expect(toggle).toHaveBeenCalledWith('speaker','double');
  });
  it.each(['en','zh-Hans'] as const)('renders warning resources and concise labels in %s',language=>{
    setLanguage(language);
    const {container}=render(<OinkProvider><DeviceItem d={device} now={0} selected={false} collapsed={false} onSelect={()=>{}}/></OinkProvider>);
    const icons=container.querySelectorAll('.dev-ico');
    expect(icons).toHaveLength(4);
    for(const icon of icons) {
      expect(icon.getAttribute('aria-label')).toContain(language==='en'?'unavailable':'不可用');
      expect(icon.getAttribute('data-tone')).toBe('warn');
      expect(icon.querySelector('[data-badge="warning"]')).not.toBeNull();
      expect(icon.getAttribute('aria-label')).not.toMatch(/state|状态/);
    }
    expect(container.querySelectorAll('.dev-ico--btn')).toHaveLength(0);
  });
});

describe('offline device row', () => {
  it.each(['en', 'zh-Hans'] as const)('separates the last-seen label and value in %s', language => {
    setLanguage(language);
    const now = new Date(2026, 5, 15, 12).getTime();
    const {container, rerender} = render(<OinkProvider><DeviceItem d={{...device, group:'offline', lastSeen:now - 3_600_000}} now={now} selected={false} collapsed={false} onSelect={()=>{}}/></OinkProvider>);
    expect(container.querySelector('.dev__seen')?.textContent).toBe(language === 'en' ? 'Last seen 1 hour ago' : '上次在线 1 小时前');
    expect(container.querySelector('.dev__hit')?.getAttribute('aria-label')).toBe(language === 'en' ? 'Remote, offline, last seen 1 hour ago' : 'Remote，离线，上次在线 1 小时前');
    rerender(<OinkProvider><DeviceItem d={{...device, group:'offline', lastSeen:now}} now={now} selected={false} collapsed={false} onSelect={()=>{}}/></OinkProvider>);
    expect(container.querySelector('.dev__seen')?.textContent).toBe(language === 'en' ? 'Last seen Just now' : '上次在线 刚刚');
    rerender(<OinkProvider><DeviceItem d={{...device, group:'offline', lastSeen:null}} now={now} selected={false} collapsed={false} onSelect={()=>{}}/></OinkProvider>);
    expect(container.querySelector('.dev__seen')?.textContent).toBe(language === 'en' ? 'Last seen Unknown' : '上次在线 未知');
  });

  it.each(['en', 'zh-Hans'] as const)('uses the same last-seen spacing in the context menu in %s', async language => {
    setLanguage(language);
    const now = new Date(2026, 5, 15, 12).getTime();
    vi.spyOn(Date, 'now').mockReturnValue(now);
    const noop = () => {};
    render(<OinkProvider defaultMotion="reduced"><SideContextMenu
      devices={[{...device, group:'offline', lastSeen:now - 3_600_000}]} requests={[]} nearby={[]}
      onShare={noop} onGameMode={noop} onPause={noop} onLeave={noop} onEditAddress={noop}
      onShowWindow={noop} onAbout={noop} onQuit={noop} onSpeaker={noop}
      onAccept={noop} onReject={noop} onPair={noop} onBlock={noop}
    ><button data-ctx="device:remote">Open device menu</button></SideContextMenu></OinkProvider>);
    fireEvent.contextMenu(screen.getByRole('button', {name:'Open device menu'}));
    expect(await screen.findByText(language === 'en' ? 'Last seen 1 hour ago' : '上次在线 1 小时前')).toBeTruthy();
  });
});
