// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeAll, expect, it, vi } from 'vitest';
import { readFileSync } from 'node:fs';
const css = readFileSync('src/devices/devices.css', 'utf8');
import { OinkProvider } from 'oink-ui';
import { DeviceItem } from './DeviceList';
import { SideContextMenu } from './DeviceMenu';
import type { Device } from './types';
import { setLanguage } from '../i18n';

const device:Device={
  id:'source',name:'Source',os:'mac',group:'self',ip:'',port:0,latency:null,lastSeen:null,
  cursor:'local',screen:'shared',mouse:'shared',keyboard:'shared',clipboard:'shared',gameMode:'off',
  speaker:{mode:'forwarding',volume:85,muted:false},
  microphone:{captureActive:true,consumerActive:true,receivingActive:true,mode:'source',volume:125,muted:false,canSetVolume:true,permissionGranted:true,virtualReady:true},
};
beforeAll(()=>Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})}));
afterEach(()=>{cleanup();setLanguage('en');});

it.each(['light','dark'] as const)('%s source glyph, send badge and volume share the accent tone',async theme=>{
  const {container}=render(<OinkProvider defaultTheme={theme}><DeviceItem d={device} now={0} selected collapsed={false} onSelect={()=>{}} onSpeaker={()=>{}} onMicrophone={()=>{}}/></OinkProvider>);
  const mic=container.querySelectorAll('.dev-ico')[3];
  expect(mic.getAttribute('data-tone')).toBe('accent');
  expect(mic.querySelector('[data-badge="send"]')).not.toBeNull();
  await waitFor(()=>expect(container.querySelectorAll('.dev__vol[data-open]')).toHaveLength(2));
  const rows=container.querySelectorAll('.vol');
  expect(rows[0].getAttribute('data-tone')).toBe('good');
  expect(rows[1].getAttribute('data-tone')).toBe('accent');
  expect(rows[1].querySelector('[role="slider"]')?.getAttribute('aria-valuemax')).toBe('200');
  // These CSS tokens are the shared theme contract; browser evidence also
  // checks the resolved glyph/badge/fill colours in light and dark themes.
  expect(css).toMatch(/\.dev-ico\[data-tone="accent"\]\s*\{\s*color: var\(--ok-accent\)/);
  expect(css).toMatch(/\.dev-badge\[data-badge="send"\]\s*\{\s*color: inherit;/);
  expect(css).toMatch(/\.vol\[data-tone="accent"\]\s*\{\s*--vol-c: var\(--ok-accent\); --vol-on: var\(--ok-on-accent\)/);
  expect(css).toContain('[data-accent="pink"] .vol[data-tone="accent"] { --vol-on: #FFFFFF; }');
  expect(css).toMatch(/\.vol__meter \.ok-meter__fill\s*\{\s*background: var\(--vol-c\)/);
});

it.each((['light','dark'] as const).flatMap(theme=>(['device','tray'] as const).map(surface=>({theme,surface}))))('$theme $surface menu uses the accent microphone row and retains controls',async({theme,surface})=>{
  setLanguage('en');const noop=()=>{}, microphone=vi.fn();
  render(<OinkProvider defaultTheme={theme} defaultMotion="reduced"><SideContextMenu devices={[device]} requests={[]} nearby={[]}
    onShare={noop} onMicrophone={microphone} onGameMode={noop} onPause={noop} onLeave={noop} onEditAddress={noop}
    onShowWindow={noop} onAbout={noop} onQuit={noop} onSpeaker={noop} onAccept={noop} onReject={noop} onPair={noop} onBlock={noop}
    onHideScreen={noop} onScreensHidden={noop}>
      <button data-ctx={surface==='tray'?'tray:':'device:source'}>Open</button>
  </SideContextMenu></OinkProvider>);
  fireEvent.contextMenu(screen.getByRole('button',{name:'Open'}));
  const input=await screen.findByRole('menuitem',{name:'Input devices'});
  input.focus();fireEvent.keyDown(input,{key:'ArrowRight'});
  const row=await screen.findByRole('group',{name:'Microphone volume for Source'});
  expect(row.getAttribute('data-tone')).toBe('accent');
  expect(document.querySelector('#devmenu-microphone-source')?.getAttribute('data-tone')).toBe('accent');
  expect(css).toContain('.devmenu__label[data-tone="accent"] .devmenu__glyph { color: var(--ok-accent); }');
  const slider=row.querySelector('[role="slider"]')!;
  expect(slider.getAttribute('aria-valuenow')).toBe('125');
  expect(slider.getAttribute('aria-valuemax')).toBe('200');
  fireEvent.keyDown(slider,{key:'ArrowRight'});
  expect(microphone).toHaveBeenCalledWith('source',{type:'volume',value:126,final:undefined,dragging:undefined});
  fireEvent.keyDown(slider,{key:'ArrowUp'});
  expect(microphone).toHaveBeenCalledWith('source',{type:'volume',value:127,final:undefined,dragging:undefined});
  fireEvent.click(screen.getByRole('button',{name:'Mute'}));
  expect(microphone).toHaveBeenCalledWith('source',{type:'mute'});
});

it('a receiving microphone carries the download badge',()=>{
  const {container}=render(<OinkProvider><DeviceItem d={{...device,microphone:{...device.microphone!,mode:'receiver'}}} now={0} selected={false} collapsed={false} onSelect={()=>{}} onSpeaker={()=>{}} onMicrophone={()=>{}}/></OinkProvider>);
  const mic=container.querySelectorAll('.dev-ico')[3];
  expect(mic.getAttribute('data-tone')).toBe('good');
  expect(mic.querySelector('[data-badge="receive"]')).not.toBeNull();
});

it.each([
  {muted:false,waitingForExit:false,badge:'send'},
  {muted:true,waitingForExit:false,badge:'mute'},
  {muted:false,waitingForExit:true,badge:'warning'},
])('forwarding speaker uses $badge and retains its existing green tone',({muted,waitingForExit,badge})=>{
  const {container}=render(<OinkProvider><DeviceItem d={{...device,speaker:{...device.speaker,muted,waitingForExit}}} now={0} selected={false} collapsed={false} onSelect={()=>{}} onSpeaker={()=>{}}/></OinkProvider>);
  const speaker=container.querySelectorAll('.dev-ico')[2];
  expect(speaker.getAttribute('data-tone')).toBe('good');
  expect(speaker.querySelector(`[data-badge="${badge}"]`)).not.toBeNull();
});

it.each(['muted','receiver','source'] as const)('only the %s microphone receives optical positioning',mode=>{
  const {container}=render(<OinkProvider><DeviceItem d={{...device,microphone:{...device.microphone!,mode}}} now={0} selected={false} collapsed={false} onSelect={()=>{}} onMicrophone={()=>{}}/></OinkProvider>);
  expect([...container.querySelectorAll('.dev-ico')].map(el=>el.getAttribute('data-glyph'))).toEqual(['mouse','keyboard','speaker','microphone']);
  expect(css).toContain('.dev-ico[data-glyph="microphone"] { translate: -2.5px; }');
});
