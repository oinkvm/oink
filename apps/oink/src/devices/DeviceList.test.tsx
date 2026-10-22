// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
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
  it.each(['local','locked'] as const)('shows only the active pointer holder for cursor=%s',cursor=>{
    const rows=(focus:number,active:boolean[]) => <OinkProvider>{active.map((gameActive,index)=><DeviceItem
      key={index} d={{...device,id:String(index),name:`Device ${index}`,group:index===0?'self':'online',
        cursor:index===focus?cursor:'remote',screen:'shared',gameMode:index===0?'on':'auto',gameActive}}
      now={0} selected={false} collapsed={false} onSelect={()=>{}}/>)}</OinkProvider>;
    const {container,rerender}=render(rows(0,[true,true]));
    expect(container.querySelectorAll('.os-game')).toHaveLength(1);
    expect(container.querySelector('[data-ctx="device:0"] .os-game')).not.toBeNull();
    expect(container.querySelector('[data-ctx="device:1"] .os-game')).toBeNull();
    // Both devices retain game mode while the pointer changes owner.
    rerender(rows(1,[true,true]));
    expect(container.querySelectorAll('.os-game')).toHaveLength(1);
    expect(container.querySelector('[data-ctx="device:0"] .os-game')).toBeNull();
    expect(container.querySelector('[data-ctx="device:1"] .os-game')).not.toBeNull();
    rerender(rows(0,[true,true]));
    expect(container.querySelector('[data-ctx="device:0"] .os-game')).not.toBeNull();
    rerender(rows(0,[false,true]));
    expect(container.querySelectorAll('.os-game')).toHaveLength(0);
    rerender(rows(-1,[true,true]));
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
      onShowWindow={noop} onAbout={noop} onQuit={noop} onSpeaker={noop} onHideScreen={noop} onScreenGain={noop} onScreensHidden={noop}
      onAccept={noop} onReject={noop} onPair={noop} onBlock={noop}
    ><button data-ctx="device:remote">Open device menu</button></SideContextMenu></OinkProvider>);
    fireEvent.contextMenu(screen.getByRole('button', {name:'Open device menu'}));
    expect(await screen.findByRole('menuitem', {name:language === 'en' ? 'Edit IP' : '编辑 IP'})).toBeTruthy();
    expect(screen.getAllByRole('menuitem')).toHaveLength(2);
    expect(screen.queryByText(language === 'en' ? 'Last seen 1 hour ago' : '上次在线 1 小时前')).toBeNull();
  });
});

describe('local audio icon', () => {
  const local = (localOutput: Device['speaker']['localOutput']): Device => ({...device, unknown:false, mouse:'shared', keyboard:'shared',
    speaker:{mode:'unused', volume:0, volumeKnown:false, muted:false, localOutput}});
  const audioIcon = (container: HTMLElement) => container.querySelector<HTMLElement>('.dev-ico[aria-label^="Audio output"]')!;
  const waves = (icon: HTMLElement) => icon.querySelectorAll('svg:first-child path:not(.fill)').length;
  it.each([
    [{volume:0, muted:false}, 0, false], [{volume:20, muted:false}, 1, false], [{volume:50, muted:false}, 2, false],
    [{volume:100, muted:false}, 3, false], [{volume:80, muted:true}, 3, true], [null, 0, false],
  ] as const)('shows the local system volume %j as %i waves (muted badge %s)', (output, count, slashed) => {
    const {container} = render(<OinkProvider><DeviceItem d={local(output)} exitName="Exit" now={0} selected={false} collapsed={false} onSelect={()=>{}} onToggle={()=>{}}/></OinkProvider>);
    const icon = audioIcon(container);
    expect(waves(icon)).toBe(count);
    expect(!!icon.querySelector('[data-badge="mute"]')).toBe(slashed);
    expect(icon.querySelectorAll('svg:first-child path.dim').length).toBe(slashed ? count : 0);
  });
  it('is disabled and not clickable without an audio exit in the group', () => {
    const toggle = vi.fn(), select = vi.fn();
    const {container} = render(<OinkProvider><DeviceItem d={local({volume:50, muted:false})} now={0} selected={false} collapsed={false} onSelect={select} onToggle={toggle}/></OinkProvider>);
    const icon = audioIcon(container);
    expect(icon.tagName).toBe('SPAN');
    expect(icon.getAttribute('aria-disabled')).toBe('true');
    fireEvent.click(icon);
    expect(toggle).not.toHaveBeenCalled();
    expect(select).not.toHaveBeenCalled();
  });
  it('switches to forwarding to the exit when clicked while an exit exists', () => {
    const toggle = vi.fn(), select = vi.fn();
    render(<OinkProvider><DeviceItem d={local({volume:50, muted:false})} exitName="Living Room" now={0} selected={false} collapsed={false} onSelect={select} onToggle={toggle}/></OinkProvider>);
    fireEvent.click(screen.getByRole('button', {name:/Audio output.*Click to forward audio to Living Room/}), {detail:1});
    expect(toggle.mock.calls).toEqual([['speaker', 'single']]);
    expect(select).not.toHaveBeenCalled();
  });
  it.each(['forwarding', 'exit'] as const)('keeps selecting the device in %s mode', mode => {
    const toggle = vi.fn(), select = vi.fn();
    render(<OinkProvider><DeviceItem d={{...device, unknown:false, speaker:{mode, volume:80, volumeKnown:true, mutedKnown:true, muted:false}}} exitName="Exit" now={0} selected={false} collapsed={false} onSelect={select} onToggle={toggle}/></OinkProvider>);
    fireEvent.click(screen.getByRole('button', {name:/Audio output.*Click to select this device and show volume/}), {detail:1});
    expect(select).toHaveBeenCalledTimes(1);
    expect(toggle).not.toHaveBeenCalled();
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


describe('Accessibility permission indicator',()=>{
  const tips = {
    en:['Accessibility permission missing, grant access','Accessibility permission missing, please grant it on that device'],
    'zh-Hans':['缺少辅助功能权限，前往授权','缺少辅助功能权限，请在该设备上授权'],
    ja:['アクセシビリティの権限がありません。アクセスを許可してください','アクセシビリティの権限がありません。そのデバイスで許可してください'],
    ko:['손쉬운 사용 권한이 없습니다. 접근을 허용하세요','손쉬운 사용 권한이 없습니다. 해당 기기에서 허용하세요'],
  };
  it.each(['en','zh-Hans','ja','ko'] as const)('replaces hidden screens immediately after the name and offers only the local grant in %s',language=>{
    setLanguage(language);
    const grant=vi.fn(), select=vi.fn();
    const row=(local:boolean,missing=true)=><OinkProvider><DeviceItem d={{...device,os:'mac',group:local?'self':'online',accessibilityMissing:missing}}
      now={0} selected={false} collapsed={false} onSelect={select} onGrantAccessibility={grant} hiddenScreens={2}/></OinkProvider>;
    const {container,rerender}=render(row(true));
    const local=screen.getByRole('button',{name:tips[language][0]});
    expect(container.querySelector('.dev__title')?.children[1]).toBe(local);
    expect(local.querySelector('svg circle')).not.toBeNull();
    expect(container.querySelector('.dev__hidden')).toBeNull();
    fireEvent.click(local);
    expect(grant).toHaveBeenCalledTimes(1);
    expect(select).not.toHaveBeenCalled();
    rerender(row(false));
    const remote=screen.getByRole('img',{name:tips[language][1]});
    expect(remote.tagName).toBe('SPAN');
    expect(remote.getAttribute('tabindex')).toBeNull();
    fireEvent.click(remote);
    expect(grant).toHaveBeenCalledTimes(1);
    rerender(row(false,false));
    expect(container.querySelector('.dev__accessibility')).toBeNull();
    expect(container.querySelector('.dev__hidden')).not.toBeNull();
  });
  it.each([true,false])('shows the actual hover tooltip for local=%s',async local=>{
    vi.useFakeTimers();
    setLanguage('en');
    try {
      const {container}=render(<OinkProvider><DeviceItem d={{...device,os:'mac',group:local?'self':'online',accessibilityMissing:true}}
        now={0} selected={false} collapsed={false} onSelect={()=>{}} onGrantAccessibility={()=>{}}/></OinkProvider>);
      const icon=container.querySelector('.dev__accessibility')!;
      fireEvent.pointerEnter(icon,{pointerType:'mouse'});
      await act(async()=>{await vi.advanceTimersByTimeAsync(1000);});
      expect(screen.getByRole('tooltip').textContent).toBe(tips.en[local?0:1]);
    } finally { cleanup();vi.useRealTimers(); }
  });
});
