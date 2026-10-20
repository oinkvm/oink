// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { PeekScope, usePeekScope } from './peek';
vi.mock('oink-ui', () => ({
  Portal: ({children}: {children: React.ReactNode}) => children,
  speed: () => 0,
  usePresence: (open: boolean) => ({mounted:open,ref:() => {},state:'entered'}),
}));
let observers: Array<{callback:ResizeObserverCallback; elements:Element[]; disconnect:ReturnType<typeof vi.fn<() => void>>}>;
let intersection: IntersectionObserverCallback;
beforeEach(() => {
  vi.useFakeTimers(); observers=[];
  vi.stubGlobal('requestAnimationFrame',vi.fn());
  vi.stubGlobal('ResizeObserver', class {
    item: typeof observers[number];
    constructor(callback:ResizeObserverCallback) {this.item={callback,elements:[],disconnect:vi.fn()}; observers.push(this.item);}
    observe(element:Element) {this.item.elements.push(element);}
    disconnect() {this.item.disconnect();}
  });
  vi.stubGlobal('IntersectionObserver', class {
    constructor(callback:IntersectionObserverCallback) {intersection=callback;}
    observe() {} disconnect() {}
  });
});
afterEach(() => {cleanup();vi.useRealTimers();vi.restoreAllMocks();vi.unstubAllGlobals();});
function Anchor() {
  const peek=usePeekScope()!;
  return <button {...peek.handlers('peer')}>Anchor</button>;
}
function mount() {
  const result=render(<div className="win"><div className="win__side"><PeekScope collapsed render={()=><div>Details</div>}><Anchor/></PeekScope></div></div>);
  const anchor=screen.getByRole('button',{name:'Anchor'});
  let top=50;
  const bounds=vi.spyOn(anchor,'getBoundingClientRect').mockImplementation(()=>({top,left:20,width:100,height:30,right:120,bottom:top+30} as DOMRect));
  fireEvent.pointerEnter(anchor);
  act(()=>vi.advanceTimersByTime(0));
  return {...result,anchor,bounds,move:(next:number)=>{top=next;}};
}
it('observes anchor changes without a display-rate layout polling loop', async () => {
  const {container,anchor,bounds,move}=mount();
  expect(screen.getByText('Details')).toBeTruthy();
  await act(async()=>{});
  const calls=bounds.mock.calls.length;
  act(()=>vi.advanceTimersByTime(10_000));
  await act(async()=>{});
  expect(bounds).toHaveBeenCalledTimes(calls);
  expect(requestAnimationFrame).not.toHaveBeenCalled();
  move(80);
  const observer=observers.find(o=>o.elements.includes(anchor))!;
  act(()=>observer.callback([],{} as ResizeObserver));
  expect((container.querySelector('.dev-peek') as HTMLElement).style.top).toBe('80px');
});
it('closes and disconnects observers when its document is hidden', () => {
  const {container}=mount();
  vi.spyOn(document,'hidden','get').mockReturnValue(true);
  fireEvent(document,new Event('visibilitychange'));
  expect(container.querySelector('.dev-peek')).toBeNull();
  expect(observers.every(o=>o.disconnect.mock.calls.length>0)).toBe(true);
});
it('closes when its anchor leaves the viewport', () => {
  const {container}=mount();
  act(()=>intersection([{isIntersecting:false,intersectionRatio:0}] as IntersectionObserverEntry[],{} as IntersectionObserver));
  expect(container.querySelector('.dev-peek')).toBeNull();
});
