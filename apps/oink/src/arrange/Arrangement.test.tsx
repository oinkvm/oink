// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, renderHook } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { useState } from 'react';
import { Arrangement } from './Arrangement';
import { cameraStore } from './cameraStore';
import type { Screen } from './geometry';
import type { Layout } from '../api/types';
import type { DeviceCardsView } from '../api/cards';
import { presentCards } from '../devices/project';
import { setLanguage } from '../i18n';
import { presentScreens, renderLayout } from './adapter';
import { useHistory } from './useHistory';

let resize: ResizeObserverCallback;
let viewport={width:900,height:600};
beforeAll(()=>{
  vi.stubGlobal('ResizeObserver',class {
    constructor(callback:ResizeObserverCallback){resize=callback;}
    observe(){} disconnect(){}
  });
  vi.stubGlobal('PointerEvent',MouseEvent);
  HTMLElement.prototype.setPointerCapture=()=>{};
  vi.spyOn(HTMLElement.prototype,'getBoundingClientRect').mockImplementation(()=>({left:0,top:0,x:0,y:0,...viewport,right:viewport.width,bottom:viewport.height,toJSON(){}}));
});

describe('arrangement computer names',()=>{
  const layout:Layout={revision:1,source:'default',geometry:{bounds:{x:0,y:0,w:3,h:1},screens:
    ['self','self','remote'].map((device,i)=>({
      key:{device,display:String(i)},device_name:device==='self'?'Local computer':'Remote computer',os:'macos',
      display:{key:String(i),name:i===1?'Monitor model':`Display ${i+1}`,bounds:{x:0,y:0,w:1920,h:1080},
        pixel_width:3840,pixel_height:2160,reference_extent:null,rotation_deg:0,is_primary:i===0,is_builtin:i===0,is_virtual:false},
      size:{w:1,h:.5625},size_source:'reported',origin:{x:i,y:0},gain:1,hidden:false,device_disabled:false,online:true,
    }))}};
  const cards:DeviceCardsView={schema:1,stream:'names',revision:'1',group:null,generatedAtMs:0,
    exitVolume:null,emergencyDelivery:null,cards:['self','remote'].map(device=>({
      device,name:device==='self'?'Local computer':'Remote computer',os:'macos',logo:'macos',
      isSelf:device==='self',section:device==='self'?'this_device':'online',lastSeenMs:null,status:null,pause:null,
      connection:{kind:'online',signal:{kind:'unknown',rttMs:null,bars:0,colour:'grey'}},
    }))};

  it.each(['zh-Hans','en'] as const)('follows device-list renames on every screen and through history in %s',language=>{
    setLanguage(language);
    const initial=renderLayout(layout,{});
    // The layout snapshot also carries computer names before cards arrive.
    expect(initial.map(s=>s.name)).toEqual(['Local computer','Local computer','Remote computer']);
    const {result}=renderHook(()=>useHistory(()=>initial));
    const renamed='Renamed computer with a name long enough to truncate on the canvas';
    const updated={...cards,cards:cards.cards.map(card=>({...card,name:card.device==='self'?renamed:card.name}))};
    let devices=presentCards(cards,[]);
    const props={onScreensChange:vi.fn(),selectedId:initial[0].id,onSelect:vi.fn(),settings};
    const {container,rerender}=render(<Arrangement {...props} screens={presentScreens(result.current.value,devices)}/>);
    expect(Array.from(container.querySelectorAll('.scr__name'),el=>el.textContent)).toEqual(devices.flatMap(d=>d.id==='self'?[d.name,d.name]:[d.name]));
    act(()=>result.current.commit(initial.map(s=>({...s,y:100})),'Move'));
    devices=presentCards(updated,[]);
    const checkNames=()=>{
      rerender(<Arrangement {...props} screens={presentScreens(result.current.value,devices)}/>);
      expect(Array.from(container.querySelectorAll('.scr__name'),el=>el.textContent)).toEqual([renamed,renamed,'Remote computer']);
      expect(Array.from(container.querySelectorAll('.scr__res'),el=>el.textContent)).toEqual(['1920 × 1080','1920 × 1080','1920 × 1080']);
      const label=container.querySelector('.scr')!.getAttribute('aria-label')!;
      expect(label.startsWith(renamed)).toBe(true);
      expect(label).toContain(language==='en'?'position':'位置');
      expect(label).not.toMatch(/Display|Monitor model/);
    };
    checkNames();
    act(()=>{result.current.undo();});
    checkNames();
    act(()=>{result.current.redo();});
    checkNames();
    fireEvent.keyDown(document.body,{key:'ArrowDown'});
    expect(props.onScreensChange.mock.lastCall?.[1].label).toBe(`${language==='en'?'Move':'移动'} ${renamed}`);
    expect(container.querySelector('[aria-live="polite"]')?.textContent).toContain(renamed);
  });
});
afterEach(()=>{cleanup();viewport={width:900,height:600};});
const screens:Screen[]=[{id:'one',name:'Display',x:0,y:0,w:1000,h:600,wallpaper:0}];
const settings={showHotZones:false,snapping:false,compact:false,padding:48,step:10};

it('switches wallpaper to a static offline surface and restores it on reconnect',()=>{
  setLanguage('en');
  const online={...screens[0],image:'wallpaper.png'};
  const props={onScreensChange:vi.fn(),selectedId:null,onSelect:vi.fn(),settings};
  const {container,rerender}=render(<Arrangement {...props} screens={[online]}/>);
  const body=container.querySelector('.scr__body') as HTMLElement;
  const label=body.querySelector('.scr__label')!.textContent;
  expect(body.style.backgroundImage).toContain('wallpaper.png');
  rerender(<Arrangement {...props} screens={[{...online,offline:true}]}/>);
  expect(body.hasAttribute('data-offline')).toBe(true);
  expect(body.style.backgroundImage).toBe('');
  expect(body.querySelector('.scr__offline')?.getAttribute('aria-hidden')).toBe('true');
  expect(body.querySelector('.scr__offline')).toMatchInlineSnapshot(`
    <svg
      aria-hidden="true"
      class="ok-icon scr__offline"
      viewBox="0 0 20 20"
    >
      <g>
        <rect
          height="9.500"
          rx="2.200"
          width="14"
          x="3"
          y="4"
        />
        <path
          d="M7.500 16.500h5M10 13.500v3"
        />
      </g>
      <path
        d="M3 3 17 17"
      />
    </svg>
  `);
  expect(body.querySelector('.scr__label')!.textContent).toBe(label);
  expect(container.querySelector('.scr')!.getAttribute('aria-label')).toMatch(/offline/i);
  rerender(<Arrangement {...props} screens={[online]}/>);
  expect(body.hasAttribute('data-offline')).toBe(false);
  expect(body.querySelector('.scr__offline')).toBeNull();
  expect(body.style.backgroundImage).toContain('wallpaper.png');
  expect(body.querySelector('.scr__label')!.textContent).toBe(label);
});

describe('arrangement camera',()=>{
  let now=0, sequence=0;
  const frames=new Map<number,FrameRequestCallback>();
  let restoreClock:()=>void;
  beforeEach(()=>{
    now=0;sequence=0;frames.clear();
    const clock=vi.spyOn(performance,'now').mockImplementation(()=>now);
    const request=vi.spyOn(globalThis,'requestAnimationFrame').mockImplementation(callback=>{
      frames.set(++sequence,callback);return sequence;
    });
    const cancel=vi.spyOn(globalThis,'cancelAnimationFrame').mockImplementation(id=>{frames.delete(id);});
    restoreClock=()=>{clock.mockRestore();request.mockRestore();cancel.mockRestore();};
  });
  afterEach(()=>{cleanup();restoreClock();});
  // Spring time is capped per frame. A wall-clock wait can expire before enough
  // frames run on a busy test worker; advance the actual animation clock instead.
  const settle=()=>{
    for(let n=0;n<240&&frames.size;n++)act(()=>{
      now+=1000/60;
      const pending=[...frames.values()];frames.clear();pending.forEach(callback=>callback(now));
    });
    expect(frames.size).toBe(0);
  };
  it('rests during a stationary held drag and pauses all frames while hidden',()=>{
    const projection=vi.fn();
    const {container,rerender}=render(<Arrangement screens={screens} onProjection={projection} onScreensChange={vi.fn()} selectedId="one" onSelect={()=>{}} settings={settings}/>);
    settle();
    const display=container.querySelector('.scr')!;
    fireEvent.pointerDown(display,{button:0,clientX:300,clientY:300});
    fireEvent.pointerMove(display,{clientX:310,clientY:300});
    settle();
    expect(frames.size).toBe(0);
    fireEvent.pointerMove(display,{clientX:320,clientY:300});
    expect(frames.size).toBeGreaterThan(0);
    const hidden=vi.spyOn(document,'hidden','get').mockReturnValue(true);
    try {
      fireEvent(document,new Event('visibilitychange'));
      expect(frames.size).toBe(0);
      expect(projection.mock.lastCall?.[0]).toBeNull();
      rerender(<Arrangement screens={screens.map(s=>({...s,w:1200}))} onProjection={projection} onScreensChange={vi.fn()} selectedId="one" onSelect={()=>{}} settings={settings}/>);
      expect(frames.size).toBe(0);
      hidden.mockReturnValue(false);
      fireEvent(document,new Event('visibilitychange'));
      settle();
    } finally {hidden.mockRestore();}
  });
  it('refits after sidebar, viewport and layout changes',()=>{
    const props={screens,onScreensChange:vi.fn(),selectedId:null,onSelect:vi.fn(),settings};
    const {rerender}=render(<Arrangement {...props}/>);
    expect(cameraStore.get()?.zoom).toBeCloseTo(.804);
    rerender(<Arrangement {...props} settings={{...settings,padding:24}}/>);
    expect(cameraStore.get()?.zoom).toBeCloseTo(.852);
    viewport={width:1000,height:700};
    act(()=>resize([{contentRect:viewport}] as ResizeObserverEntry[],{} as ResizeObserver));
    expect(cameraStore.get()?.zoom).toBeCloseTo(.952);
    rerender(<Arrangement {...props} screens={screens.map(s=>({...s,w:2000}))}/>);
    settle();
    expect(cameraStore.get()?.zoom).toBeCloseTo(.452);
  });
  it('preserves camera continuity on pointer events and smoothly refits after release',()=>{
    const changed=vi.fn();
    const {container}=render(<Arrangement screens={screens} onScreensChange={changed} selectedId="one" onSelect={()=>{}} settings={settings}/>);
    const display=container.querySelector('.scr')!;
    const original=cameraStore.get()!;
    fireEvent.pointerDown(display,{button:0,clientX:300,clientY:300});
    fireEvent.pointerMove(display,{clientX:420,clientY:330});
    expect(cameraStore.get()?.zoom).toBe(original.zoom);
    expect(cameraStore.get()?.fit.x).toBeGreaterThan(0);
    const at=cameraStore.get()!;
    viewport={width:1200,height:800};
    act(()=>resize([{contentRect:viewport}] as ResizeObserverEntry[],{} as ResizeObserver));
    expect(cameraStore.get()?.zoom).toBe(original.zoom);
    fireEvent.pointerMove(display,{clientX:440,clientY:330});
    expect(cameraStore.get()!.fit.x-at.fit.x).toBeCloseTo(Math.round(20/original.zoom),0);
    fireEvent.pointerUp(display,{clientX:440,clientY:330});
    expect(changed).toHaveBeenCalledOnce();
    expect(cameraStore.get()?.zoom).toBe(original.zoom);
    settle();
    expect(cameraStore.get()?.zoom).toBeGreaterThan(original.zoom);
    expect(container.querySelector('.arr')?.getAttribute('data-interacting')).toBeNull();
  });

  it('updates contact radii on the painted element for WebKit geometry invalidation',()=>{
    const props={screens,onScreensChange:vi.fn(),selectedId:null,onSelect:vi.fn(),settings};
    const {container,rerender}=render(<Arrangement {...props}/>);
    const body=container.querySelector('.scr__body') as HTMLElement;
    expect(body.style.getPropertyValue('--r-ne')).toBe('12px');
    expect(body.parentElement!.style.getPropertyValue('--r-ne')).toBe('');
    rerender(<Arrangement {...props} screens={[...screens,{...screens[0],id:'two',x:1000}]}/>);
    settle();
    expect(body.style.getPropertyValue('--r-ne')).toBe('0px');
    expect(body.style.getPropertyValue('--r-se')).toBe('0px');
    expect(body.style.getPropertyValue('--r-nw')).toBe('12px');
  });

  it('removes a missing wallpaper without overriding the opaque theme surface',()=>{
    const props={onScreensChange:vi.fn(),selectedId:null,onSelect:vi.fn(),settings};
    const {container,rerender}=render(<Arrangement {...props} screens={[{...screens[0],image:'wallpaper.png'}]}/>);
    const body=container.querySelector('.scr__body') as HTMLElement;
    expect(body.style.backgroundImage).toContain('wallpaper.png');
    rerender(<Arrangement {...props} screens={screens}/>);
    expect(body.style.backgroundImage).toBe('');
    expect(body.style.backgroundColor).toBe('');
  });
});

describe('rigid computer selection and gestures', () => {
  const group: Screen[] = [
    { id: 'a', device: 'computer', name: 'Computer', x: 0, y: 0, w: 1000, h: 600, wallpaper: 0 },
    { id: 'b', device: 'computer', name: 'Computer', x: 1000, y: 200, w: 800, h: 500, wallpaper: 1 },
    { id: 'c', device: 'other', name: 'Other', x: 2200, y: 0, w: 1000, h: 600, wallpaper: 2 },
  ];

  it('rounds scaled group dimensions and positions in accessible screen labels', () => {
    setLanguage('en');
    const scaled = group.map(screen => ({
      ...screen,
      name: 'Another computer',
      x: 3949.2,
      y: -123.4,
      w: 974.4,
      h: 1670.3999999999999,
    }));
    const { getAllByRole } = render(
      <Arrangement
        screens={scaled}
        selectedId="a"
        onSelect={vi.fn()}
        onScreensChange={vi.fn()}
        settings={settings}
      />,
    );
    expect(getAllByRole('button')[0].getAttribute('aria-label')).toBe(
      'Another computer, 974 × 1670, position 3949, −123',
    );
  });

  it('selects the computer from either screen and shows one bounding selection border', () => {
    const select = vi.fn();
    const props = { screens: group, onScreensChange: vi.fn(), onSelect: select, settings };
    const { container, rerender } = render(<Arrangement {...props} selectedId={null} />);
    expect(container.querySelectorAll('.scr__group-border')).toHaveLength(0);
    // Multi-screen computers have no per-screen corner hit areas, even on hover.
    expect(container.querySelectorAll('.scr:not(.scr--group) .scr__hz')).toHaveLength(4);
    fireEvent.pointerDown(container.querySelectorAll('.scr')[1], { button: 0, clientX: 400, clientY: 200 });
    expect(select).toHaveBeenLastCalledWith('b');
    fireEvent.pointerUp(container.querySelectorAll('.scr')[1]);
    rerender(<Arrangement {...props} selectedId="b" />);
    const selected = container.querySelectorAll('[aria-pressed="true"]');
    expect(selected).toHaveLength(2);
    expect(container.querySelectorAll('.scr__group-border')).toHaveLength(1);
    expect(container.querySelectorAll('.scr--group .scr__hz')).toHaveLength(4);
    expect(container.querySelectorAll('.scr:not(.scr--group) .scr__hz')).toHaveLength(4);
    const border = container.querySelector('.scr--group') as HTMLElement;
    const members=Array.from(container.querySelectorAll('.scr:not(.scr--group)')).slice(0,2).map(el=>{
      const style=(el as HTMLElement).style;
      const [x,y]=style.transform.match(/-?[\d.]+/g)!.map(Number);
      return {x,y,right:x+parseFloat(style.width),bottom:y+parseFloat(style.height)};
    });
    expect(parseFloat(border.style.width)).toBe(Math.max(...members.map(m=>m.right))-Math.min(...members.map(m=>m.x)));
    expect(parseFloat(border.style.height)).toBe(Math.max(...members.map(m=>m.bottom))-Math.min(...members.map(m=>m.y)));
  });

  it('dragging either member translates every member and leaves other computers fixed', () => {
    const changed = vi.fn();
    const { container } = render(
      <Arrangement
        screens={group}
        onScreensChange={changed}
        selectedId="b"
        onSelect={() => {}}
        settings={settings}
      />,
    );
    const screen = container.querySelectorAll('.scr')[1];
    fireEvent.pointerDown(screen, { button: 0, clientX: 350, clientY: 240 });
    fireEvent.pointerMove(screen, { clientX: 370, clientY: 260 });
    fireEvent.pointerUp(screen);
    const next = changed.mock.lastCall![0] as Screen[];
    expect(next[1].x - next[0].x).toBeCloseTo(1000);
    expect(next[1].y - next[0].y).toBeCloseTo(200);
    expect(next[0].x).not.toBe(0);
    expect(next[0].w).toBe(1000);
    expect(next[2]).toEqual(group[2]);
  });

  it('a bounding corner scales every member uniformly around its opposite group corner', () => {
    const changed = vi.fn();
    const { container } = render(
      <Arrangement
        screens={group}
        onScreensChange={changed}
        selectedId="a"
        onSelect={() => {}}
        settings={settings}
      />,
    );
    const handle = container.querySelector('.scr--group .scr__hz[data-c="se"]')!;
    const zoom = cameraStore.get()!.zoom;
    fireEvent.pointerDown(handle, { button: 0, clientX: 400, clientY: 300 });
    fireEvent.pointerMove(handle, { clientX: 400 + 40 * zoom, clientY: 300 + 20 * zoom });
    fireEvent.pointerUp(handle);
    const next = changed.mock.lastCall![0] as Screen[];
    const k = next[0].w / group[0].w;
    expect(k).toBeGreaterThan(1);
    expect(next[1].w / group[1].w).toBeCloseTo(k);
    expect(next[1].h / group[1].h).toBeCloseTo(k);
    expect(next[1].x - next[0].x).toBeCloseTo(1000 * k);
    expect(next[1].y - next[0].y).toBeCloseTo(200 * k);
    expect(next[2]).toEqual(group[2]);
  });
});

it('cancels a group gesture when live OS geometry changes or a member is unplugged', () => {
  const screens: Screen[] = [
    { id: 'a', device: 'one', name: 'One', x: 0, y: 0, w: 1000, h: 600, wallpaper: 0 },
    { id: 'b', device: 'one', name: 'One', x: 1000, y: 200, w: 800, h: 500, wallpaper: 1 },
  ];
  const props = { screens, selectedId: 'a', onSelect: vi.fn(), onScreensChange: vi.fn(), settings };
  const { container, rerender } = render(<Arrangement {...props} />);
  const screen = container.querySelector('.scr')!;
  fireEvent.pointerDown(screen, { button: 0, clientX: 300, clientY: 300 });
  fireEvent.pointerMove(screen, { clientX: 330, clientY: 320 });
  expect(container.querySelector('.arr')?.getAttribute('data-interacting')).toBe('move');
  rerender(<Arrangement {...props} screens={[screens[1]]} />);
  expect(container.querySelector('.arr')?.getAttribute('data-interacting')).toBeNull();
  fireEvent.pointerUp(container.querySelector('.scr')!);
  expect(props.onScreensChange).not.toHaveBeenCalled();
});

it.each([false, true])('moves only the released computer and keeps a nook release whose closure would change a projection, Shift=%s', shiftKey => {
  // One wide upper screen and two lower screens with staggered top edges.
  const screens: Screen[] = [
    { id: 'top', device: 'a', name: 'Upper', x: 508, y: 0, w: 868, h: 488, wallpaper: 0 },
    { id: 'left', device: 'b', name: 'Lower left', x: 0, y: 525, w: 558, h: 361, wallpaper: 1 },
    { id: 'right', device: 'c', name: 'Lower right', x: 737, y: 543, w: 584, h: 377, wallpaper: 2 },
  ];
  const changed = vi.fn();
  const { container } = render(
    <Arrangement
      screens={screens}
      selectedId="right"
      onSelect={vi.fn()}
      onScreensChange={changed}
      settings={{ ...settings, compact: true }}
    />,
  );
  const display = container.querySelectorAll('.scr')[2];
  const zoom = cameraStore.get()!.zoom;
  fireEvent.pointerDown(display, { button: 0, clientX: 500, clientY: 350, shiftKey });
  fireEvent.pointerMove(display, { clientX: 500 + 10 * zoom, clientY: 350, shiftKey });
  fireEvent.pointerUp(display, { shiftKey });
  const next = changed.mock.lastCall![0] as Screen[];
  // Rising to the upper screen would move its seam with the lower-left screen;
  // sliding left would move its interval under the upper screen.
  expect(next[0]).toEqual(screens[0]);
  expect(next[1]).toEqual(screens[1]);
  expect(next[2].x).toBeCloseTo(747);
  expect(next[2].y).toBe(543);
});

it('ignores hidden screens: they neither block nor attract the released computer', () => {
  const all: Screen[] = [
    { id: 'a', device: 'a', name: 'A', x: 0, y: 0, w: 1000, h: 600, wallpaper: 0 },
    { id: 'h', device: 'h', name: 'Hidden', x: 1500, y: 0, w: 1000, h: 600, wallpaper: 1 },
    { id: 'b', device: 'b', name: 'B', x: 3000, y: 0, w: 1000, h: 600, wallpaper: 2 },
  ];
  const changed = vi.fn();
  const { container } = render(<Arrangement screens={[all[0], all[2]]} geometryScreens={all} selectedId="b"
    onSelect={vi.fn()} onScreensChange={changed} settings={{ ...settings, compact: true }}/>);
  const display = container.querySelectorAll('.scr')[1];
  const zoom = cameraStore.get()!.zoom;
  fireEvent.pointerDown(display, { button: 0, clientX: 700, clientY: 300 });
  // Released overlapping the hidden screen: no collision and no contact with it.
  fireEvent.pointerMove(display, { clientX: 700 - 1200 * zoom, clientY: 300 });
  fireEvent.pointerUp(display);
  const next = changed.mock.lastCall![0] as Screen[];
  expect(next[0]).toEqual(all[0]);
  expect(next[1]).toEqual(all[1]);
  expect(next[2].x).toBeCloseTo(1000);
});


describe('canvas key hints', () => {
  const props = { screens, onScreensChange: vi.fn(), onSelect: vi.fn(), settings };
  beforeEach(() => { viewport = { width: 900, height: 700 }; });

  it.each([
    ['zh-Hans', '移动', '缩放', '微调'], ['en', 'Move', 'Resize', 'Fine'],
    ['ja', '移動', 'サイズ変更', '微調整'], ['ko', '이동', '크기 조절', '미세 조절'],
  ])('shows localized hints only for a selected screen in %s', (language, move, resize, fine) => {
    setLanguage(language);
    const { container, rerender } = render(<Arrangement {...props} selectedId={null}/>);
    expect(container.querySelector('.arr__keys')).toBeNull();
    rerender(<Arrangement {...props} selectedId="one"/>);
    const hints = container.querySelector('.arr__keys')!;
    expect(hints.textContent).toBe(`↑↓←→${move}·⌘${resize}·⇧${fine}`);
    expect(hints.querySelectorAll('kbd')).toHaveLength(6);
    rerender(<Arrangement {...props} selectedId="one" platform="win"/>);
    expect(container.querySelector('.arr__keys')?.textContent).toContain(`Ctrl${resize}`);
    expect(container.querySelector('.arr__keys')?.textContent).not.toContain('⌘');
    rerender(<Arrangement {...props} selectedId="missing"/>);
    expect(container.querySelector('.arr__keys')).toBeNull();
  });

  it('hides rather than covering screens or an active projection gesture', () => {
    const { container, rerender } = render(<Arrangement {...props} selectedId="one"/>);
    expect(container.querySelector('.arr__keys')).not.toBeNull();
    fireEvent.pointerDown(container.querySelector('.scr')!, { button: 0, clientX: 300, clientY: 300 });
    expect(container.querySelector('.arr__keys')).not.toBeNull();
    fireEvent.pointerMove(container.querySelector('.scr')!, { clientX: 330, clientY: 300 });
    expect(container.querySelector('.arr__keys')).toBeNull();
    fireEvent.pointerUp(container.querySelector('.scr')!);
    expect(container.querySelector('.arr__keys')).not.toBeNull();
    // A tall display fills the existing top whitespace. Do not shift the camera.
    rerender(<Arrangement {...props} screens={[{...screens[0], w: 600, h: 1600}]} selectedId="one"/>);
    viewport = { width: 600, height: 400 };
    act(() => resize([{contentRect: viewport}] as ResizeObserverEntry[], {} as ResizeObserver));
    expect(container.querySelector('.arr__keys')).toBeNull();
  });

  it('keeps the same hint mounted while a click switches the selection', () => {
    setLanguage('en');
    const pair: Screen[] = [screens[0], { ...screens[0], id: 'two', name: 'Second', x: 1000 }];
    function Host() {
      const [selected, setSelected] = useState<string | null>('one');
      return <Arrangement {...props} screens={pair} selectedId={selected} onSelect={setSelected}/>;
    }
    const { container } = render(<Host/>);
    const hints = container.querySelector('.arr__keys')!;
    const text = hints.textContent;
    expect(text).toBe('↑↓←→Move·⌘Resize·⇧Fine');
    const added: Node[] = [], removed: Node[] = [];
    const observer = new MutationObserver(records => records.forEach(r => {
      added.push(...r.addedNodes);
      removed.push(...r.removedNodes);
    }));
    observer.observe(container, { childList: true, subtree: true });
    const second = container.querySelectorAll('.scr')[1];
    fireEvent.pointerDown(second, { button: 0, clientX: 650, clientY: 350 });
    // A press that has not moved is a click, not a projection gesture.
    expect(container.querySelector('.arr__keys')).toBe(hints);
    fireEvent.pointerUp(second, { clientX: 650, clientY: 350 });
    observer.takeRecords().forEach(r => { added.push(...r.addedNodes); removed.push(...r.removedNodes); });
    observer.disconnect();
    expect(second.getAttribute('aria-pressed')).toBe('true');
    expect(container.querySelector('.arr__keys')).toBe(hints);
    expect(hints.textContent).toBe(text);
    expect([...added, ...removed].filter(n => n === hints || n.contains(hints) || hints.contains(n))).toEqual([]);
  });

  it.each([['ArrowRight', 100, 0], ['ArrowDown', 0, 100], ['ArrowLeft', -100, 0], ['ArrowUp', 0, -100]])(
    'keeps the advertised %s movement and Shift fine step', (key, x, y) => {
      const change = vi.fn();
      render(<Arrangement {...props} onScreensChange={change} selectedId="one"/>);
      fireEvent.keyDown(document.body, { key });
      expect(change.mock.lastCall?.[0][0]).toMatchObject({ x, y });
      fireEvent.keyDown(document.body, { key, shiftKey: true });
      expect(change.mock.lastCall?.[0][0]).toMatchObject({ x: x / 10, y: y / 10 });
    });

  it.each(['metaKey', 'ctrlKey'])('keeps %s resize, Shift fine resize and Escape deselection', modifier => {
    const change = vi.fn(), select = vi.fn();
    render(<Arrangement {...props} onScreensChange={change} onSelect={select} selectedId="one"/>);
    fireEvent.keyDown(document.body, { key: 'ArrowRight', [modifier]: true });
    expect(change.mock.lastCall?.[0][0].w).toBe(1100);
    fireEvent.keyDown(document.body, { key: 'ArrowRight', [modifier]: true, shiftKey: true });
    expect(change.mock.lastCall?.[0][0].w).toBe(1020);
    fireEvent.keyDown(document.body, { key: 'Escape' });
    expect(select).toHaveBeenCalledWith(null);
  });
});

it('keeps fractional world coordinates through a pointer move and commit', () => {
  const source = { ...screens[0], x: 604.25, y: 472.8125 };
  const changed = vi.fn();
  const { container } = render(<Arrangement screens={[source]} selectedId={source.id}
    onSelect={vi.fn()} onScreensChange={changed} settings={settings} />);
  const display = container.querySelector('.scr')!;
  const zoom = cameraStore.get()!.zoom;
  fireEvent.pointerDown(display, { button: 0, clientX: 400, clientY: 300 });
  fireEvent.pointerMove(display, { clientX: 400 + 10 * zoom, clientY: 300 });
  fireEvent.pointerUp(display);
  const next = (changed.mock.lastCall![0] as Screen[])[0];
  expect(next.x).toBeCloseTo(source.x + 10, 10);
  expect(next.y).toBeCloseTo(source.y, 10);
});

describe('auto-compaction follows manual edits only', () => {
  // Two independent computers with a closable 100-unit gap.
  const gap: Screen[] = [
    { id: 'a', device: 'a', name: 'A', x: 0, y: 0, w: 1000, h: 600, wallpaper: 0 },
    { id: 'b', device: 'b', name: 'B', x: 1100, y: 0, w: 1000, h: 600, wallpaper: 1 },
  ];
  const opening = (next: Screen[]) => next[1].x - (next[0].x + next[0].w);
  const compact = { ...settings, compact: true };
  beforeEach(() => setLanguage('en'));

  it('does not compact when screens are added, removed, hidden, go offline or arrive from another device', () => {
    const changed = vi.fn();
    const props = { onScreensChange: changed, selectedId: null, onSelect: vi.fn(), settings: compact };
    const { rerender } = render(<Arrangement {...props} screens={gap}/>);
    const plugged: Screen = { id: 'c', device: 'c', name: 'C', x: 2300, y: 0, w: 800, h: 500, wallpaper: 2 };
    // Hotplug: a new screen with its own gap, then an unplugged screen.
    rerender(<Arrangement {...props} screens={[...gap, plugged]}/>);
    rerender(<Arrangement {...props} screens={[gap[0]]}/>);
    // A hidden or disabled screen keeps only its retained geometry.
    rerender(<Arrangement {...props} screens={[gap[0]]} geometryScreens={gap}/>);
    rerender(<Arrangement {...props} screens={gap}/>);
    // Connectivity changes and a remote layout that opens a wider gap.
    rerender(<Arrangement {...props} screens={[gap[0], { ...gap[1], offline: true }]}/>);
    rerender(<Arrangement {...props} screens={[gap[0], { ...gap[1], x: 1400 }]}/>);
    rerender(<Arrangement {...props} screens={gap}/>);
    expect(changed).not.toHaveBeenCalled();
  });

  it.each([true, false])('a keyboard move compacts the whole layout only when compaction=%s', on => {
    const changed = vi.fn();
    render(<Arrangement screens={gap} selectedId="b" onSelect={vi.fn()} onScreensChange={changed}
      settings={{ ...settings, compact: on }}/>);
    fireEvent.keyDown(document.body, { key: 'ArrowRight' });
    const next = changed.mock.lastCall![0] as Screen[];
    expect(opening(next)).toBeCloseTo(on ? 0 : 200);
    expect(changed.mock.lastCall![1]).toEqual({ label: 'Move B', merge: 'key-move:b' });
  });

  it.each([true, false])('a keyboard resize compacts only when compaction=%s', on => {
    const changed = vi.fn();
    render(<Arrangement screens={gap} selectedId="a" onSelect={vi.fn()} onScreensChange={changed}
      settings={{ ...settings, compact: on }}/>);
    fireEvent.keyDown(document.body, { key: 'ArrowLeft', metaKey: true });
    const next = changed.mock.lastCall![0] as Screen[];
    expect(next[0].w).toBeCloseTo(1000 / 1.1);
    expect(opening(next) > 1).toBe(!on);
  });

  it('a pointer release keeps the gap when compaction is off', () => {
    const changed = vi.fn();
    const { container } = render(<Arrangement screens={gap} selectedId="b" onSelect={vi.fn()}
      onScreensChange={changed} settings={settings}/>);
    const display = container.querySelectorAll('.scr')[1];
    const zoom = cameraStore.get()!.zoom;
    fireEvent.pointerDown(display, { button: 0, clientX: 600, clientY: 300 });
    fireEvent.pointerMove(display, { clientX: 600 + 20 * zoom, clientY: 300 });
    fireEvent.pointerUp(display);
    const next = changed.mock.lastCall![0] as Screen[];
    expect(next[0]).toEqual(gap[0]);
    expect(next[1].x).toBeCloseTo(1120);
  });
});
