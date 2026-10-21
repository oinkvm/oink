// @vitest-environment jsdom
import { cleanup, render } from '@testing-library/react';
import { afterEach, beforeAll, expect, it } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { VolumeRow } from './VolumeRow';
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
it('keeps known volume visible while a command is pending', () => {
  const {container} = render(<OinkProvider><VolumeRow d={{...device,pending:true}} open tabbable onSpeaker={()=>{}}/></OinkProvider>);
  expect(container.querySelector('[role="slider"]')?.getAttribute('aria-valuenow')).toBe('80');
  expect(container.querySelector('[role="slider"]')?.getAttribute('aria-disabled')).toBe('true');
});
