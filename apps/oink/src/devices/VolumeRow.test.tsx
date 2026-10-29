// @vitest-environment jsdom
import { cleanup, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { VolumeRow } from './VolumeRow';
import type { SpeakerAction } from './VolumeRow';
import type { Device } from './types';

beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', {value: () => ({matches:false, addEventListener(){}, removeEventListener(){}})});
});
afterEach(cleanup);
const device: Device = {
  id:'peer', name:'Peer', os:'win', group:'online', ip:'', port:0, latency:null,
  lastSeen:null, cursor:'remote', screen:'shared', mouse:'shared', keyboard:'shared',
  clipboard:'shared', gameMode:'auto',
  speaker:{mode:'forwarding', volume:80, volumeKnown:true, mutedKnown:true, muted:false},
};
it.each([
  {volumeKnown:false}, {mutedKnown:false}, {volume:NaN}, {mode:'unknown' as const},
  {mode:'fault' as const}, {mode:'unused' as const},
])('omits the volume row without valid audio state: %j', speaker => {
  const {container} = render(<OinkProvider><VolumeRow d={{...device, speaker:{...device.speaker,...speaker}}} open tabbable onSpeaker={()=>{}}/></OinkProvider>);
  expect(container.querySelector('.dev__vol')).toBeNull();
});
it('keeps an optimistic volume visible and enabled', () => {
  const {container} = render(<OinkProvider><VolumeRow d={device} open tabbable onSpeaker={()=>{}}/></OinkProvider>);
  expect(container.querySelector('[role="slider"]')?.getAttribute('aria-valuenow')).toBe('80');
  expect(container.querySelector('[role="slider"]')?.getAttribute('aria-disabled')).toBeNull();
});

describe('live volume', () => {
  beforeAll(() => {
    vi.stubGlobal('PointerEvent', MouseEvent);
    HTMLElement.prototype.setPointerCapture = () => {};
  });
  const slider = (onSpeaker: (a: SpeakerAction) => void, d: Device = device) => {
    const view = render(<OinkProvider><VolumeRow d={d} open tabbable onSpeaker={onSpeaker}/></OinkProvider>);
    const el = view.container.querySelector<HTMLElement>('[role="slider"]')!;
    el.getBoundingClientRect = () => ({ left: 0, top: 0, x: 0, y: 0, width: 200, height: 20, right: 200, bottom: 20, toJSON() {} });
    return { ...view, el };
  };
  it('applies while dragging and sends the final value on release', () => {
    const onSpeaker = vi.fn();
    const { el } = slider(onSpeaker);
    fireEvent.pointerDown(el, { button: 0, clientX: 100, pointerId: 1 });
    fireEvent.pointerMove(el, { clientX: 120, pointerId: 1 });
    fireEvent.pointerMove(el, { clientX: 120, pointerId: 1 });
    fireEvent.pointerMove(el, { clientX: 150, pointerId: 1 });
    expect(el.getAttribute('aria-valuenow')).toBe('148');
    fireEvent.pointerUp(el, { pointerId: 1 });
    // Pointer gain is mapped around the 100% detent; repeated positions are not resent.
    expect(onSpeaker.mock.calls.map(([a]) => [a.value, a.final ?? false])).toEqual([[100, false], [118, false], [148, false], [148, true]]);
  });
  it('applies keyboard and wheel steps at once', () => {
    const onSpeaker = vi.fn();
    const { el } = slider(onSpeaker);
    fireEvent.keyDown(el, { key: 'ArrowRight' });
    fireEvent.keyDown(el, { key: 'ArrowRight' });
    fireEvent.wheel(el, { deltaY: -4 });
    fireEvent.wheel(el, { deltaY: 4, shiftKey: true });
    expect(onSpeaker.mock.calls.map(([a]) => a.value)).toEqual([81, 82, 83, 73]);
  });
});

it('does not disable or remount audio controls when another device resource changes', () => {
  const row = (local: boolean) => <OinkProvider><VolumeRow d={{...device,mouse:local?'local':'shared'}} open tabbable onSpeaker={()=>{}}/></OinkProvider>;
  const {container,rerender} = render(row(false));
  const button = container.querySelector('button')!;
  const glyph = container.querySelector('.vol__glyph')!;
  rerender(row(true));
  expect(container.querySelector('button')).toBe(button);
  expect(container.querySelector('.vol__glyph')).toBe(glyph);
  expect(button.disabled).toBe(false);
  expect(container.querySelector('[role="slider"]')?.getAttribute('aria-disabled')).toBeNull();
});

it.each([false,true])('keeps microphone controls enabled while its mute state changes (%s)', muted => {
  const d:Device={...device,microphone:{mode:'source',permissionGranted:true,virtualReady:true,volume:100,muted,canSetVolume:true}};
  const {container}=render(<OinkProvider><VolumeRow d={d} microphone open tabbable onSpeaker={()=>{}}/></OinkProvider>);
  expect(container.querySelector('button')!.disabled).toBe(false);
});
