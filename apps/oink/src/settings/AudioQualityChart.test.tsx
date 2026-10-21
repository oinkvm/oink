// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { setLanguage } from '../i18n';
import type {AudioQuality} from '../api/types';
import { AudioQualityChart } from './AudioQualityChart';
const mocks=vi.hoisted(()=>({query:vi.fn(),listen:vi.fn()}));
vi.mock('../api/client',()=>({api:{audioQuality:mocks.query},onAgentEvent:mocks.listen}));
beforeEach(()=>{setLanguage('en');vi.stubGlobal('PointerEvent',MouseEvent);mocks.listen.mockResolvedValue(vi.fn());vi.stubGlobal('ResizeObserver',class{observe(){}disconnect(){}});});
afterEach(()=>{cleanup();vi.clearAllMocks();vi.unstubAllGlobals();});
it('renders per-source buffer lines and window statistics',async()=>{
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:60000,sources:[{device:'peer',name:'Source',samples:[{at_ms:59750,buffer_ms:5},{at_ms:60000,buffer_ms:7}],p50_ms:5,p90_ms:7,p99_ms:8,bitrate_kbps:320,dropouts:2,glitches:[]}]});
 const {container}=render(<AudioQualityChart active/>);
 await screen.findByRole('cell',{name:'Source'});
 expect(screen.getAllByRole('columnheader').map(x=>x.textContent)).toEqual(['Device','Buffer P50','Buffer P90','Buffer P99','Average bitrate','Dropouts']);
 expect(screen.getAllByRole('cell').map(x=>x.textContent)).toEqual(['Source','5.00 ms','7.00 ms','8.00 ms','320.00 kbps','2']);
 expect(container.querySelector('.lat__line')?.getAttribute('d')).toContain('L');
});
it.each([['en','No audio received'],['zh-Hans','尚未接收音频'],['ja','受信したオーディオはありません'],['ko','수신한 오디오가 없습니다']] as const)('renders the empty state in %s',async (language,message)=>{
 setLanguage(language);mocks.query.mockResolvedValue({revision:1,sampled_at_ms:60000,sources:[]});
 const {container}=render(<AudioQualityChart active/>);
 await waitFor(()=>expect(mocks.query).toHaveBeenCalled());
 expect(container.querySelector('.lat__empty')?.textContent).toBe(message);
 expect(screen.queryAllByRole('columnheader')).toHaveLength(0);
 expect(screen.queryByRole('table')).toBeNull();
 expect(screen.queryByRole('tooltip')).toBeNull();
});

it('adopts source starts/stops and ignores an older query after a live tick',async()=>{
 let resolve!:(value:AudioQuality)=>void;
 mocks.query.mockImplementationOnce(()=>new Promise<AudioQuality>(r=>{resolve=r;}));
 let deliver!:(event:unknown)=>void;
 mocks.listen.mockImplementationOnce(async(handler:(event:unknown)=>void)=>{deliver=handler;return vi.fn();});
 const {container}=render(<AudioQualityChart active/>);
 await waitFor(()=>expect(deliver).toBeDefined());
 const source={device:'peer',name:'Source',samples:[{at_ms:59750,buffer_ms:5},{at_ms:60000,buffer_ms:7}],p50_ms:5,p90_ms:7,p99_ms:8,bitrate_kbps:320,dropouts:2,glitches:[]};
 act(()=>deliver({type:'audio_quality',data:{revision:2,sampled_at_ms:60000,sources:[source]}}));
 await screen.findByRole('cell',{name:'Source'});
 await act(async()=>resolve({revision:1,sampled_at_ms:59000,sources:[]}));
 expect(container.querySelectorAll('.lat__line')).toHaveLength(1);
 act(()=>deliver({type:'audio_quality',data:{revision:3,sampled_at_ms:120000,sources:[]}}));
 expect(screen.queryByRole('cell',{name:'Source'})).toBeNull();
 expect(container.querySelector('.lat__empty')?.textContent).toBe('No audio received');
});

it('draws only dropout seconds and lists two devices with their series colours in one tooltip',async()=>{
 const source={device:'a',name:'Alpha',samples:[{at_ms:59000,buffer_ms:5}],p50_ms:5,p90_ms:5,p99_ms:5,bitrate_kbps:320,dropouts:1,glitches:[{at_ms:59000,lost:1,late:0}]};
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:60000,sources:[source,{...source,device:'b',name:'Beta',glitches:[{at_ms:59000,lost:0,late:1}]},{...source,device:'c',name:'Clear',dropouts:0,glitches:[]}]});
 const {container}=render(<AudioQualityChart active/>);
 await screen.findByRole('cell',{name:'Alpha'});
 expect(container.querySelectorAll('.lat__glitch')).toHaveLength(1);
 fireEvent.click(screen.getByRole('button',{name:'Hide Alpha in chart'}));
 expect(container.querySelectorAll('.lat__glitch')).toHaveLength(1);
 const hit=container.querySelector('.lat__glitch-hit')!;
 fireEvent.pointerMove(hit,{clientX:470,clientY:50});
 const tip=await screen.findByRole('tooltip');
 const rows=tip.querySelectorAll('.lat__tip-row');
 expect([...rows].map(row=>row.textContent)).toEqual(['Alpha','Beta']);
 for(const row of rows){
   const cell=screen.getByRole('cell',{name:row.textContent!});
   expect(row.querySelector('i')?.getAttribute('style')).toBe(cell.querySelector('i')?.getAttribute('style'));
 }
 expect(tip.textContent).not.toContain('Clear');
});
