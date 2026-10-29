// @vitest-environment jsdom
import { act, cleanup, render } from '@testing-library/react';
import { afterEach, beforeAll, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { DeviceItem } from './DeviceList';
import { agent, useAgent } from '../api/store';
import { setLanguage } from '../i18n';
import type { DeviceCard, DeviceCardsView } from '../api/cards';
import type { Device } from './types';
const volume=vi.hoisted(()=>vi.fn());
vi.mock('./VolumeRow',()=>({hasVolume:()=>true,VolumeRow:()=>{volume();return <div data-testid="volume"/>;}}));
beforeAll(()=>{Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})});});
afterEach(()=>{cleanup();agent.reset();setLanguage('en');volume.mockClear();});
it('updates RTT text, signal bars and accessible labels without rendering the page or volume controls',()=>{
  setLanguage('en');
  const card:DeviceCard={device:'peer',name:'Peer',os:'windows',logo:'windows10',isSelf:false,section:'online',lastSeenMs:null,status:null,pause:null,connection:{kind:'online',signal:{kind:'measured',rttMs:3,bars:3,colour:'green'}}};
  const cards:DeviceCardsView={schema:1,stream:'s',revision:'1',group:null,generatedAtMs:0,exitVolume:null,emergencyDelivery:null,cards:[card]};
  agent.receive({type:'device_cards',data:cards});
  let renders=0;
  const d:Device={id:'peer',name:'Peer',os:'win',group:'online',ip:'',port:0,latency:3,lastSeen:null,cursor:'remote',screen:'shared',mouse:'shared',keyboard:'shared',clipboard:'shared',speaker:{mode:'forwarding',volume:100,muted:false},gameMode:null};
  function Page(){useAgent();renders++;return <DeviceItem d={d} now={0} selected={false} collapsed={false} onSelect={()=>{}} onSpeaker={()=>{}}/>;}
  const {container}=render(<OinkProvider><Page/></OinkProvider>);
  const before={renders,volume:volume.mock.calls.length};
  act(()=>agent.receive({type:'device_card_signals',data:{stream:'s',revision:'1',sampledAtMs:1000,signals:[{device:'peer',rttMs:52.4}]}}));
  expect(container.querySelector('.dev-conn__num')?.textContent).toBe('52');
  expect(container.querySelector('.dev-conn .dev-signal')?.getAttribute('data-tone')).toBe('poor');
  expect(container.querySelectorAll('.dev-conn .dev-signal [data-on]')).toHaveLength(1);
  expect(container.querySelector('.dev__hit')?.getAttribute('aria-label')).toContain('52.4');
  expect(container.querySelector('.dev-conn')?.getAttribute('aria-label')).toContain('52.4');
  expect(renders).toBe(before.renders);expect(volume).toHaveBeenCalledTimes(before.volume);
  act(()=>agent.receive({type:'device_card_signals',data:{stream:'s',revision:'1',sampledAtMs:2000,signals:[{device:'peer',rttMs:null}]}}));
  expect(container.querySelector('.dev-conn__num')?.textContent).toBe('—');
});
