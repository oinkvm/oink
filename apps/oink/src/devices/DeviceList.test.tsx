// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
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
it('shows the lock badge only during the locked owner’s pointer visit',()=>{
  setLanguage('en');
  const row=(cursor:Device['cursor'])=><OinkProvider><DeviceItem d={{...device,cursor,screen:'shared'}} now={0} selected={false} collapsed={false} onSelect={()=>{}}/></OinkProvider>;
  const {container,rerender}=render(row('locked'));
  expect(container.querySelector('[aria-label*="pointer locked here"]')).not.toBeNull();
  rerender(row('remote'));
  expect(container.querySelector('[aria-label*="pointer locked here"]')).toBeNull();
  rerender(row('locked'));
  expect(container.querySelector('[aria-label*="pointer locked here"]')).not.toBeNull();
});
describe('unavailable device row',()=>{
  it.each(['unknown','fault'] as const)('selects the %s device through audio without changing its audio route',mode=>{
    const toggle=vi.fn(), select=vi.fn();
    render(<OinkProvider><DeviceItem d={{...device,speaker:{...device.speaker,mode}}} now={0} selected={false} collapsed={false} onSelect={select} onToggle={toggle}/></OinkProvider>);
    const output=screen.getByRole('button',{name:/Audio output/});
    fireEvent.click(output,{detail:1});
    fireEvent.click(output,{detail:2});
    expect(select).toHaveBeenCalledTimes(1);
    expect(toggle).not.toHaveBeenCalled();
  });
  it('disables an available keyboard through double click and shift-enter',()=>{
    const toggle=vi.fn();
    render(<OinkProvider><DeviceItem d={{...device,unknown:false,keyboard:'shared'}} now={0} selected={false} collapsed={false} onSelect={()=>{}} onToggle={toggle}/></OinkProvider>);
    const key=screen.getByRole('button',{name:/Keyboard/});
    fireEvent.click(key,{detail:1});
    fireEvent.click(key,{detail:2});
    fireEvent.keyDown(key,{key:'Enter',shiftKey:true});
    expect(toggle.mock.calls).toEqual([['keyboard','single'],['keyboard','double'],['keyboard','double']]);
  });
  it('marks disabled keyboard and mouse with the ban badge',()=>{
    setLanguage('en');
    const {container}=render(<OinkProvider><DeviceItem d={{...device,keyboard:'disabled',mouse:'disabled'}} now={0} selected={false} collapsed={false} onSelect={()=>{}}/></OinkProvider>);
    for(const name of ['Keyboard: disabled','Mouse: disabled']){
      const icon=container.querySelector(`.dev-ico[aria-label="${name}"]`);
      expect(icon?.querySelector('[data-badge="ban"] circle'),name).not.toBeNull();
      expect(icon?.getAttribute('data-tone'),name).toBe('bad');
    }
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
    expect(icons).toHaveLength(4);
    expect(icons[3].getAttribute("data-disabled")).toBeNull();
    for(const icon of Array.from(icons).slice(0,3)) {
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
  it('can become the exit even when the group has none', () => {
    const toggle = vi.fn();
    const {container} = render(<OinkProvider><DeviceItem d={local({volume:50, muted:false})} now={0} selected={false} collapsed={false} onSelect={()=>{}} onToggle={toggle}/></OinkProvider>);
    const icon = audioIcon(container);
    expect(icon.tagName).toBe('BUTTON');
    fireEvent.click(icon,{detail:1});
    expect(toggle.mock.calls).toEqual([['speaker','single']]);
    fireEvent.click(icon,{detail:2});
    expect(toggle.mock.calls).toEqual([['speaker','single'],['speaker','double']]);
  });
  it('selects and switches to forwarding to the exit when clicked while an exit exists', () => {
    const toggle = vi.fn(), select = vi.fn();
    render(<OinkProvider><DeviceItem d={local({volume:50, muted:false})} exitName="Living Room" now={0} selected={false} collapsed={false} onSelect={select} onToggle={toggle}/></OinkProvider>);
    fireEvent.click(screen.getByRole('button', {name:/Audio output.*Click: Send audio/}), {detail:1});
    expect(toggle.mock.calls).toEqual([['speaker', 'single']]);
    expect(select).toHaveBeenCalledTimes(1);
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
    en:['Accessibility permission missing, grant access','Accessibility permission missing'],
    'zh-Hans':['缺少辅助功能权限，前往授权','缺少辅助功能权限'],
    ja:['アクセシビリティの権限がありません。アクセスを許可してください','アクセシビリティ権限がありません'],
    ko:['손쉬운 사용 권한이 없습니다. 접근을 허용하세요','손쉬운 사용 권한 없음'],
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

describe('microphone controls and explicit volume expansion',()=>{
  const source:Device={...device,unknown:false,speaker:{mode:'forwarding',volume:100,muted:false},
    microphone:{mode:'source',volume:100,muted:false,permissionGranted:true,virtualReady:true,canSetVolume:true}};
  it('selection displays speaker above microphone, and deselection collapses both',async()=>{
    const row=(selected:boolean)=><OinkProvider><DeviceItem d={source} now={0} selected={selected} collapsed={false} onSelect={()=>{}} onSpeaker={()=>{}} onMicrophone={()=>{}}/></OinkProvider>;
    const {container,rerender}=render(row(true));
    await waitFor(()=>expect(container.querySelectorAll('.dev__vol[data-open]')).toHaveLength(2));
    expect(screen.getAllByRole('group').map(e=>e.getAttribute('aria-label'))).toEqual(['Audio output volume for Remote','Microphone volume for Remote']);
    expect(screen.getAllByRole('slider').map(e=>e.getAttribute('aria-valuemax'))).toEqual(['200','200']);
    rerender(row(false));
    expect(container.querySelectorAll('.dev__vol[data-open]')).toHaveLength(0);
  });
  it.each(['receiver','muted','unavailable'] as const)('routes %s microphone clicks without opening volume',mode=>{
    const action=vi.fn();
    const {container}=render(<OinkProvider><DeviceItem d={{...source,group:mode==='unavailable'?'self':'online',microphone:{...source.microphone!,mode}}} now={0} selected collapsed={false} onSelect={()=>{}} onMicrophone={action}/></OinkProvider>);
    const mic=screen.getByRole('button',{name:/Microphone:/});
    fireEvent.click(mic,{detail:1});
    expect(action).toHaveBeenCalledWith(mode==='receiver'?'local':'receive');
    expect(container.querySelectorAll('.dev__vol[data-open]')).toHaveLength(0);
  });
});

describe('disabled input restore', () => {
  it.each(['keyboard','mouse'] as const)('%s: single and double click both only restore sharing', kind => {
    const toggle=vi.fn();
    render(<OinkProvider><DeviceItem d={{...device,unknown:false,[kind]:'disabled',canDisableMouse:true}} now={0} selected collapsed={false} onSelect={()=>{}} onToggle={toggle}/></OinkProvider>);
    const icon=document.querySelectorAll('.dev-ico')[kind==='mouse'?0:1];
    fireEvent.click(icon,{detail:1});
    fireEvent.click(icon,{detail:2});
    fireEvent.doubleClick(icon,{detail:2});
    fireEvent.keyDown(icon,{key:'Enter',shiftKey:true});
    expect(toggle.mock.calls).toEqual([[kind,'single']]);
  });
  it.each(['keyboard','mouse'] as const)('%s: the second click does not disable after the first restored sharing', kind => {
    const toggle=vi.fn();
    const item=(mode:'disabled'|'shared')=><OinkProvider><DeviceItem d={{...device,unknown:false,[kind]:mode,canDisableMouse:true}} now={0} selected collapsed={false} onSelect={()=>{}} onToggle={toggle}/></OinkProvider>;
    const {rerender}=render(item('disabled'));
    const icon=()=>document.querySelectorAll('.dev-ico')[kind==='mouse'?0:1];
    fireEvent.click(icon(),{detail:1});
    // The restore applies before the second click of the same double-click arrives.
    rerender(item('shared'));
    fireEvent.click(icon(),{detail:2});
    fireEvent.doubleClick(icon(),{detail:2});
    expect(toggle.mock.calls).toEqual([[kind,'single']]);
  });
});

describe('immediate audio status actions', () => {
  it.each(['unused','forwarding','exit'] as const)('speaker %s: first click now, second click only its own action, keyboard parity', mode => {
    vi.useFakeTimers();
    try {
      const toggle=vi.fn(), mute=vi.fn();
      render(<OinkProvider><DeviceItem d={{...device, speaker:{mode,volume:60,muted:false}}} now={0} selected collapsed={false} onSelect={()=>{}} onToggle={toggle} onSpeaker={mute}/></OinkProvider>);
      const icon=screen.getByRole('button',{name:/Audio output/});
      // Exit and forwarding mute on click; forwarding stops sharing (a single toggle) on double-click.
      const muteFirst=mode!=='unused';
      const second=mode==='forwarding'?['speaker','single']:['speaker','double'];
      fireEvent.click(icon,{detail:1});
      // No clock advancement and no second event required.
      expect(muteFirst?mute:toggle).toHaveBeenCalledTimes(1);
      expect(muteFirst?mute.mock.calls:toggle.mock.calls).toEqual(muteFirst?[[{type:'mute'}]]:[['speaker','single']]);
      fireEvent.click(icon,{detail:2});
      fireEvent.doubleClick(icon,{detail:2});
      expect(toggle.mock.calls.at(-1)).toEqual(second);
      expect(mute).toHaveBeenCalledTimes(muteFirst?1:0);
      expect(toggle).toHaveBeenCalledTimes(muteFirst?1:2);
      fireEvent.keyDown(icon,{key:'Enter',shiftKey:true});
      expect(toggle.mock.calls.at(-1)).toEqual(second);
      expect(toggle).toHaveBeenCalledTimes(muteFirst?2:3);
      fireEvent.click(icon,{detail:0}); // native keyboard activation (Enter/Space)
      expect(muteFirst?mute:toggle).toHaveBeenCalledTimes(muteFirst?2:4);
    } finally { vi.useRealTimers(); }
  });
  it.each(['idle','receiver','muted','source','unavailable'] as const)('microphone %s: no delay, no extra toggle, Shift+Enter parity',mode=>{
    vi.useFakeTimers();
    try {
      const action=vi.fn();
      render(<OinkProvider><DeviceItem d={{...device,group:mode==='unavailable'?'self':'online',microphone:{mode,volume:100,muted:mode==='muted',permissionGranted:true,virtualReady:mode!=='unavailable',canSetVolume:mode==='source'}}} now={0} selected collapsed={false} onSelect={()=>{}} onMicrophone={action}/></OinkProvider>);
      const icon=document.querySelectorAll('.dev-ico')[3];
      fireEvent.click(icon,{detail:1});
      const first=mode==='source'?{type:'mute'}:mode==='unavailable'||mode==='muted'?'receive':'local';
      const second=mode==='source'?'local':'shared';
      expect(action.mock.calls).toEqual([[first]]);
      fireEvent.click(icon,{detail:2});
      fireEvent.doubleClick(icon,{detail:2});
      expect(action.mock.calls).toEqual([[first],[second]]);
      fireEvent.keyDown(icon,{key:'Enter',shiftKey:true});
      expect(action).toHaveBeenCalledTimes(3);
      expect(action.mock.calls.at(-1)).toEqual([second]);
    } finally { vi.useRealTimers(); }
  });
  it.each(['en','zh-Hans','ja','ko'] as const)('tooltips describe both actions in %s',async language=>{
    setLanguage(language);
    const {container}=render(<OinkProvider><DeviceItem d={{...device,speaker:{mode:'exit',volume:50,muted:false}}} now={0} selected collapsed={false} onSelect={()=>{}} onToggle={()=>{}} onSpeaker={()=>{}}/></OinkProvider>);
    const icon=container.querySelectorAll<HTMLButtonElement>('.dev-ico')[2];
    const words={en:'Double-click', 'zh-Hans':'双击', ja:'ダブルクリック', ko:'두 번 클릭'};
    expect(icon.getAttribute('aria-label')).toContain(words[language]);
    fireEvent.pointerEnter(icon,{pointerType:'mouse'});
    await waitFor(()=>expect(screen.getByRole('tooltip').textContent).toContain(words[language]));
  });
});
