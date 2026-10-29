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
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:60000,output:null,sources:[{device:'peer',name:'Source',samples:[{at_ms:59750,buffer_ms:5},{at_ms:60000,buffer_ms:7}],buffer_avg_ms:6,bitrate_kbps:320,interruptions:2,concealed_ms:60,late_packets:12,glitches:[]}]});
 const {container}=render(<AudioQualityChart span={60} active/>);
 await screen.findByRole('cell',{name:'Source'});
 expect(screen.getAllByRole('columnheader').map(x=>x.textContent)).toEqual(['Device','Buffer Avg','Average bitrate','Interruptions','Concealed audio','Late packets','Discontinuities']);
 // Plain header, no explanation tooltip; the table scrolls with the project's scrollbar.
 expect(screen.getAllByRole('columnheader')[3].children).toHaveLength(0);
 expect(container.querySelector('.lat__table-scroll')?.classList.contains('ok-scroll')).toBe(true);
 expect(screen.getAllByRole('cell').map(x=>x.textContent)).toEqual(['Source','6.00 ms','320.00 kbps','2','60.00 ms','12','0']);
 expect(container.querySelector('.lat__line')?.getAttribute('d')).toContain('L');
});
it('labels the interruptions column 中断 in Chinese',async()=>{
 setLanguage('zh-Hans');
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:60000,output:null,sources:[{device:'peer',name:'Source',samples:[{at_ms:60000,buffer_ms:7}],buffer_avg_ms:6,bitrate_kbps:320,interruptions:2,concealed_ms:60,late_packets:12,glitches:[]}]});
 render(<AudioQualityChart span={60} active/>);
 await screen.findByRole('cell',{name:'Source'});
 expect(screen.getAllByRole('columnheader')[3].textContent).toBe('中断');
});
it.each([['en','No audio received'],['zh-Hans','尚未接收音频'],['ja','受信したオーディオはありません'],['ko','수신한 오디오가 없습니다']] as const)('renders the empty state in %s',async (language,message)=>{
 setLanguage(language);mocks.query.mockResolvedValue({revision:1,sampled_at_ms:60000,output:null,sources:[]});
 const {container}=render(<AudioQualityChart span={60} active/>);
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
 const {container}=render(<AudioQualityChart span={60} active/>);
 await waitFor(()=>expect(deliver).toBeDefined());
 const source={device:'peer',name:'Source',samples:[{at_ms:59750,buffer_ms:5},{at_ms:60000,buffer_ms:7}],buffer_avg_ms:6,bitrate_kbps:320,interruptions:2,concealed_ms:60,late_packets:12,glitches:[]};
 act(()=>deliver({type:'audio_quality',data:{revision:2,sampled_at_ms:60000,output:null,sources:[source]}}));
 await screen.findByRole('cell',{name:'Source'});
 await act(async()=>resolve({revision:1,sampled_at_ms:59000,output:null,sources:[]}));
 expect(container.querySelectorAll('.lat__line')).toHaveLength(1);
 act(()=>deliver({type:'audio_quality',data:{revision:3,sampled_at_ms:120000,output:null,sources:[]}}));
 expect(screen.queryByRole('cell',{name:'Source'})).toBeNull();
 expect(container.querySelector('.lat__empty')?.textContent).toBe('No audio received');
});

it('draws only dropout seconds and lists two devices with their series colours in one tooltip',async()=>{
 const source={device:'a',name:'Alpha',samples:[{at_ms:59000,buffer_ms:5}],buffer_avg_ms:5,bitrate_kbps:320,interruptions:1,concealed_ms:5,late_packets:1,glitches:[{at_ms:59000,interruptions:1}]};
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:60000,output:null,sources:[source,{...source,device:'b',name:'Beta',glitches:[{at_ms:59000,interruptions:1}]},{...source,device:'c',name:'Clear',interruptions:0,glitches:[]}]});
 const {container}=render(<AudioQualityChart span={60} active/>);
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

it('shows corrected packets without red markers and attributes native underruns only to output', async()=>{
 const bucket={at_ms:59000,interruptions:0,concealed_ns:60000000,missing_ns:0,late_packets:12,silence_interruptions:0,concealment_interruptions:0,discontinuities:0};
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:60000,sources:[{device:'peer',name:'Source',samples:[{at_ms:59000,buffer_ms:5}],buffer_avg_ms:5,bitrate_kbps:320,interruptions:0,concealed_ms:60,late_packets:12,glitches:[bucket]}],output:{interruptions:1,missing_ms:10,glitches:[{...bucket,at_ms:58000,interruptions:1}]}});
 const {container}=render(<AudioQualityChart span={60} active/>);
 await screen.findByRole('cell',{name:'Source'});
 expect(container.querySelectorAll('.lat__glitch')).toHaveLength(1);
 expect(container.querySelector('.lat__glitch-hit')?.getAttribute('data-glitch-second')).toBe('58000');
 expect(screen.getByRole('cell',{name:'Local processing'})).toBeTruthy();
 expect(screen.getByRole('cell',{name:'60.00 ms'})).toBeTruthy();
 expect(screen.getByRole('cell',{name:'12'})).toBeTruthy();
});

it('re-arms at a zero-ratio intersection edge and receives the replacement source session',async()=>{
 let visibility!:(entries:Partial<IntersectionObserverEntry>[])=>void;
 vi.stubGlobal('IntersectionObserver',class{constructor(cb:typeof visibility){visibility=cb;}observe(){}disconnect(){}});
 let deliver!:(event:unknown)=>void;
 const stop=vi.fn();
 mocks.listen.mockImplementation(async(handler:(event:unknown)=>void)=>{deliver=handler;return stop;});
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:0,sources:[],output:null});
 const {container}=render(<AudioQualityChart span={60} active/>);
 // Default threshold 0 sends this edge contact, then need not send another
 // callback as the chart scrolls further into view. A ratio>0 gate freezes it.
 act(()=>visibility([{isIntersecting:true,intersectionRatio:0}]));
 await waitFor(()=>expect(mocks.query).toHaveBeenCalledTimes(1));
 const source={device:'old',name:'Old source',samples:[{at_ms:1000,buffer_ms:5}],buffer_avg_ms:5,bitrate_kbps:320,interruptions:0,concealed_ms:0,late_packets:0,glitches:[]};
 act(()=>deliver({type:'audio_quality',data:{revision:2,sampled_at_ms:1000,sources:[source],output:null}}));
 await screen.findByRole('cell',{name:'Old source'});
 act(()=>visibility([{isIntersecting:false,intersectionRatio:0}]));
 expect(stop).toHaveBeenCalledTimes(1);
 act(()=>visibility([{isIntersecting:true,intersectionRatio:0}]));
 await waitFor(()=>expect(mocks.query).toHaveBeenCalledTimes(2));
 act(()=>deliver({type:'audio_quality',data:{revision:3,sampled_at_ms:2000,sources:[{...source,device:'new',name:'New source',samples:[{at_ms:2000,buffer_ms:10}]}],output:null}}));
 await screen.findByRole('cell',{name:'New source'});
 expect(screen.queryByRole('cell',{name:'Old source'})).toBeNull();
 expect(container.querySelectorAll('.lat__line')).toHaveLength(1);
});

it('selects backend source and local-processing statistics and groups markers at absolute starts',async()=>{
 const bucket={at_ms:164000,interruptions:1,concealed_ns:0,missing_ns:0,late_packets:0,silence_interruptions:0,concealment_interruptions:0,discontinuities:0};
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:302000,sources:[{device:'peer',name:'Source',samples:[{at_ms:170250,buffer_ms:4},{at_ms:174750,buffer_ms:18},{at_ms:301750,buffer_ms:6},{at_ms:302000,buffer_ms:7}],buffer_avg_ms:6.5,bitrate_kbps:20,interruptions:0,concealed_ms:2,late_packets:3,stats_5m:{buffer_avg_ms:12,bitrate_kbps:88,interruptions:4,concealed_ms:22,late_packets:33},glitches:[bucket,{...bucket,at_ms:163000}]}],output:{interruptions:0,missing_ms:0,stats_5m:{interruptions:9,missing_ms:11},glitches:[bucket]}});
 const {container,rerender}=render(<AudioQualityChart active/>);
 await screen.findByRole('cell',{name:'Source'});
 expect(screen.getAllByRole('row')[1].textContent).toContain('12.00 ms88.00 kbps422.00 ms33');
 expect(screen.getAllByRole('row')[2].textContent).toContain('9');
 expect(container.querySelectorAll('.lat__glitch')).toHaveLength(1);
 expect(container.querySelector('.lat__glitch-hit')?.getAttribute('data-glitch-second')).toBe('160000');
 fireEvent.pointerMove(container.querySelector('.lat__plot')!,{clientX:64+(1-132/300)*408});
 expect(container.querySelector('.lat__tip-row b')?.textContent).toBe('18.00 ms');
 rerender(<AudioQualityChart active span={60}/>);
 expect(screen.getAllByRole('row')[1].textContent).toContain('6.50 ms20.00 kbps02.00 ms3');
 expect(container.querySelectorAll('.lat__glitch')).toHaveLength(0);
 expect(container.querySelector('.lat__line')?.getAttribute('d')?.match(/L/g)).toHaveLength(1);
});

it('surfaces discontinuities even when the duration threshold reports no interruption',async()=>{
 const bucket={at_ms:59000,interruptions:0,concealed_ns:5000000,missing_ns:0,late_packets:1,silence_interruptions:0,concealment_interruptions:0,discontinuities:3};
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:60000,output:null,sources:[{device:'peer',name:'Source',samples:[{at_ms:59000,buffer_ms:5}],buffer_avg_ms:5,bitrate_kbps:320,interruptions:0,output_interruptions:0,discontinuities:3,concealed_ms:5,late_packets:1,glitches:[bucket]}]});
 const {container}=render(<AudioQualityChart active/>);
 await screen.findByRole('cell',{name:'Source'});
 expect(screen.getByRole('columnheader',{name:'Discontinuities'})).toBeTruthy();
 expect(screen.getByRole('cell',{name:'3'})).toBeTruthy();
 expect(container.querySelectorAll('.lat__glitch')).toHaveLength(1);
});

 it('uses backend discontinuity and attribution totals for each window',async()=>{
 const bucket={at_ms:100000,interruptions:1,output_interruptions:1,concealed_ns:0,missing_ns:0,late_packets:0,silence_interruptions:0,concealment_interruptions:0,discontinuities:2};
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:200000,sources:[{device:'peer',name:'Source',samples:[],buffer_avg_ms:null,bitrate_kbps:0,interruptions:0,output_interruptions:0,discontinuities:0,concealed_ms:0,late_packets:0,stats_5m:{buffer_avg_ms:null,bitrate_kbps:0,interruptions:1,output_interruptions:1,discontinuities:2,concealed_ms:0,late_packets:0},glitches:[bucket]}],output:{interruptions:0,missing_ms:0,discontinuities:0,stats_5m:{interruptions:1,missing_ms:2,discontinuities:3},glitches:[bucket]}});
 const {container,rerender}=render(<AudioQualityChart active span={300}/>);
 await screen.findByRole('cell',{name:'Source'});
 expect(screen.getAllByRole('row')[1].lastElementChild?.textContent).toBe('2');
 expect(screen.getAllByRole('row')[2].lastElementChild?.textContent).toBe('3');
 expect(screen.getByRole('cell',{name:'Local processing'})).toBeTruthy();
 fireEvent.pointerEnter(screen.getAllByRole('row')[1].querySelectorAll('td')[3].querySelector('span')!);
 expect(await screen.findByText('1 occurred in the local output queue and affected this source; this does not indicate a sender fault.')).toBeTruthy();
 rerender(<AudioQualityChart active span={60}/>);
 expect(screen.getAllByRole('row')[1].lastElementChild?.textContent).toBe('0');
 expect(screen.getAllByRole('row')[2].lastElementChild?.textContent).toBe('0');
 expect(screen.getAllByRole('row')[1].querySelectorAll('td')[3].querySelector('span')).toBeNull();
 expect(container.querySelectorAll('.lat__glitch')).toHaveLength(0);
 });

it.each([
 ['en','Speaker','Microphone'],
 ['zh-Hans','扬声器','麦克风'],
 ['ja','スピーカー','マイク'],
 ['ko','스피커','마이크'],
] as const)('distinguishes speaker and microphone histories from the same device in %s',async(language,speaker,microphone)=>{
 setLanguage(language);
 const source={device:'peer',name:'Source',samples:[{at_ms:59000,buffer_ms:5},{at_ms:60000,buffer_ms:7}],buffer_avg_ms:6,bitrate_kbps:120,interruptions:1,concealed_ms:25,late_packets:5,glitches:[]};
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:60000,output:null,sources:[{...source,kind:'speaker'},{...source,kind:'microphone',samples:[{at_ms:1000,buffer_ms:5},{at_ms:2000,buffer_ms:5}],bitrate_kbps:5,interruptions:0,concealed_ms:0,late_packets:0}]});
 const {container}=render(<AudioQualityChart span={60} active/>);
 const speakerCell=await screen.findByRole('cell',{name:`Source · ${speaker}`});
 const microphoneCell=screen.getByRole('cell',{name:`Source · ${microphone}`});
 expect(speakerCell.closest('tr')?.textContent).toContain('120.00 kbps125.00 ms5');
 expect(microphoneCell.closest('tr')?.textContent).toContain('5.00 kbps00.00 ms0');
 expect(container.querySelectorAll('.lat__line')).toHaveLength(2);
 expect(container.querySelector('[data-device="peer:speaker"]')?.getAttribute('stroke')).not.toBe(container.querySelector('[data-device="peer:microphone"]')?.getAttribute('stroke'));
 expect(screen.getByRole('img').getAttribute('aria-label')).toContain(`Source · ${speaker}`);
 expect(screen.getByRole('img').getAttribute('aria-label')).toContain(`Source · ${microphone}`);
 // A retained, stopped stream still has a distinct name and independent toggle.
 fireEvent.click(microphoneCell.querySelector('.lat__name')!);
 expect(container.querySelectorAll('.lat__line')).toHaveLength(1);
 expect(container.querySelector('.lat__line')?.getAttribute('data-device')).toBe('peer:speaker');
});

it('uses stream labels in hover, interruption tooltips and legend controls',async()=>{
 const source={device:'peer',name:'Source',samples:[{at_ms:59000,buffer_ms:5},{at_ms:60000,buffer_ms:7}],buffer_avg_ms:6,bitrate_kbps:120,interruptions:1,concealed_ms:25,late_packets:5,glitches:[{at_ms:58000,interruptions:1}]};
 mocks.query.mockResolvedValue({revision:1,sampled_at_ms:60000,output:null,sources:[{...source,kind:'speaker'},{...source,kind:'microphone'}]});
 const {container}=render(<AudioQualityChart span={60} active/>);
 await screen.findByRole('cell',{name:'Source · Speaker'});
 fireEvent.pointerMove(container.querySelector('.lat__plot')!,{clientX:472,clientY:50});
 expect([...container.querySelectorAll('.lat__tip-row span')].map(x=>x.textContent)).toEqual(['Source · Speaker','Source · Microphone']);
 fireEvent.pointerMove(container.querySelector('.lat__glitch-hit')!,{clientX:458,clientY:50});
 const tip=await screen.findByRole('tooltip');
 expect([...tip.querySelectorAll('.lat__tip-row span')].map(x=>x.textContent)).toEqual(['Source · Speaker','Source · Microphone']);
 fireEvent.click(screen.getByRole('button',{name:'Show only Source · Microphone in chart'}));
 expect(container.querySelectorAll('.lat__line')).toHaveLength(1);
 expect(container.querySelector('.lat__line')?.getAttribute('data-device')).toBe('peer:microphone');
 fireEvent.click(screen.getByRole('button',{name:'Show all devices in chart'}));
 fireEvent.click(screen.getByRole('button',{name:'Hide Source · Speaker in chart'}));
 expect(container.querySelectorAll('.lat__line')).toHaveLength(1);
 expect(container.querySelector('.lat__line')?.getAttribute('data-device')).toBe('peer:microphone');
});
