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
  it('selects the device through audio without changing its audio route',()=>{
    const toggle=vi.fn(), select=vi.fn();
    render(<OinkProvider><DeviceItem d={device} now={0} selected={false} collapsed={false} onSelect={select} onToggle={toggle}/></OinkProvider>);
    const output=screen.getByRole('button',{name:/Audio output/});
    fireEvent.click(output,{detail:1});
    fireEvent.click(output,{detail:2});
    expect(select).toHaveBeenCalledTimes(1);
    expect(toggle).not.toHaveBeenCalled();
  });
  it('never disables an input through double click or shift-enter',()=>{
    const toggle=vi.fn();
    render(<OinkProvider><DeviceItem d={{...device,unknown:false,keyboard:'shared'}} now={0} selected={false} collapsed={false} onSelect={()=>{}} onToggle={toggle}/></OinkProvider>);
    const key=screen.getByRole('button',{name:/Keyboard/});
    fireEvent.click(key,{detail:1});
    fireEvent.click(key,{detail:2});
    fireEvent.keyDown(key,{key:'Enter',shiftKey:true});
    expect(toggle.mock.calls).toEqual([['keyboard','single']]);
  });
  it('does not show a selected device while collapsed',()=>{
    const {container}=render(<OinkProvider><DeviceItem d={device} now={0} selected collapsed onSelect={()=>{}}/></OinkProvider>);
    expect(container.querySelector('[data-current]')).toBeNull();
    expect(container.querySelector('[aria-current]')).toBeNull();
  });
  it.each(['en','zh-Hans'] as const)('renders warning resources and concise labels in %s',language=>{
    setLanguage(language);
    const {container}=render(<OinkProvider><DeviceItem d={device} now={0} selected={false} collapsed={false} onSelect={()=>{}}/></OinkProvider>);
    const icons=container.querySelectorAll('.dev-ico');
    expect(icons).toHaveLength(3);
    for(const icon of icons) {
      expect(icon.getAttribute('aria-label')).toContain(language==='en'?'unavailable':'不可用');
      expect(icon.getAttribute('data-tone')).toBe('warn');
      expect(icon.querySelector('[data-badge="warning"]')).not.toBeNull();
      expect(icon.getAttribute('aria-label')).not.toMatch(/state|状态/);
    }
    expect(container.querySelectorAll('.dev-ico--btn')).toHaveLength(1);
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

  it.each(['en', 'zh-Hans'] as const)('offers only edit IP and removal for an offline device in %s', async language => {
    setLanguage(language);
    const now = new Date(2026, 5, 15, 12).getTime();
    vi.spyOn(Date, 'now').mockReturnValue(now);
    const noop = () => {};
    render(<OinkProvider defaultMotion="reduced"><SideContextMenu
      devices={[{...device, group:'offline', lastSeen:now - 3_600_000}]} requests={[]} nearby={[]}
      onShare={noop} onGameMode={noop} onPause={noop} onLeave={noop} onEditAddress={noop}
      onShowWindow={noop} onAbout={noop} onQuit={noop} onSpeaker={noop} onHideScreen={noop} onScreensHidden={noop}
      onAccept={noop} onReject={noop} onPair={noop} onBlock={noop}
    ><button data-ctx="device:remote">Open device menu</button></SideContextMenu></OinkProvider>);
    fireEvent.contextMenu(screen.getByRole('button', {name:'Open device menu'}));
    expect(await screen.findByRole('menuitem', {name:language === 'en' ? 'Edit IP' : '编辑 IP'})).toBeTruthy();
    expect(screen.getAllByRole('menuitem')).toHaveLength(2);
    expect(screen.queryByText(language === 'en' ? 'Last seen 1 hour ago' : '上次在线 1 小时前')).toBeNull();
  });
});

describe('hidden screens mark', () => {
  it.each(['en', 'zh-Hans'] as const)('marks a device with hidden screens next to its name in %s', language => {
    setLanguage(language);
    const row = (hiddenScreens: number, group: Device['group'] = 'online') => <OinkProvider><DeviceItem d={{...device, unknown:false, group, screen:'shared'}} hiddenScreens={hiddenScreens} now={0} selected={false} collapsed={false} onSelect={()=>{}}/></OinkProvider>;
    const {container, rerender} = render(row(0));
    expect(container.querySelector('.dev__hidden')).toBeNull();
    rerender(row(1));
    expect(container.querySelector('.dev__title .dev__hidden')?.getAttribute('aria-label')).toBe(language === 'en' ? '1 screen hidden' : '1 块屏幕已隐藏');
    rerender(row(2, 'offline'));
    expect(container.querySelector('.dev__title .dev__hidden')?.getAttribute('aria-label')).toBe(language === 'en' ? '2 screens hidden' : '2 块屏幕已隐藏');
  });
});
