// @vitest-environment jsdom
import {afterEach,beforeAll,expect,it,vi} from 'vitest';
import {cleanup,fireEvent,render,screen,waitFor} from '@testing-library/react';
import {OinkProvider} from 'oink-ui';
import {agent} from '../api/store';
import {api} from '../api/client';
import type {AudioStatus} from '../api/types';
import {setLanguage} from '../i18n';
import {MicrophoneInput} from './MicrophoneInput';
import {searchSections,sectionsFor} from './schema';
beforeAll(()=>{
 Object.defineProperty(window,'matchMedia',{value:()=>({matches:true,addEventListener(){},removeEventListener(){}}),configurable:true});
 vi.stubGlobal('ResizeObserver',class {observe(){} unobserve(){} disconnect(){}});
 HTMLElement.prototype.scrollIntoView=()=>{};
});
afterEach(()=>{cleanup();vi.restoreAllMocks();agent.adopt('audio',null);setLanguage('en');});
const input={id:'coreaudio:hardware',name:'Hardware microphone'};
function adopt(id:string|null,inputs=[input]) {agent.adopt('audio',{microphone_input:id,microphone_inputs:inputs} as unknown as AudioStatus);}
it('lists hardware and follow-default, saves through existing input option, and follows hotplug',async()=>{
 setLanguage('zh-Hans'); adopt(null);
 const write=vi.spyOn(api,'setMicrophoneInput').mockResolvedValue(null);
 vi.spyOn(api,'audioStatus').mockResolvedValue({microphone_input:input.id,microphone_inputs:[input]} as unknown as AudioStatus);
 render(<OinkProvider><MicrophoneInput/></OinkProvider>);
 fireEvent.click(screen.getByRole('combobox'));
 fireEvent.click(await screen.findByRole('option',{name:input.name}));
 await waitFor(()=>expect(write).toHaveBeenCalledWith(input.id));
 adopt(input.id,[]);
 await waitFor(()=>expect(screen.getByRole('combobox').textContent).toContain('所选麦克风不可用'));
 expect(write).toHaveBeenCalledTimes(1);
 adopt(input.id);
 await waitFor(()=>expect(screen.getByRole('combobox').textContent).toContain(input.name));
 fireEvent.click(screen.getByRole('combobox'));
 fireEvent.click(await screen.findByRole('option',{name:'跟随系统默认'}));
 await waitFor(()=>expect(write).toHaveBeenCalledWith(null));
});
it.each([['zh-Hans','跟随系统默认','麦克风'],['en','Follow system default','microphone'],['ja','システムのデフォルトに従う','マイク'],['ko','시스템 기본값 따르기','마이크']] as const)('places the selector last and makes it searchable in %s',(lang,label,keyword)=>{
 setLanguage(lang);adopt(null);
 render(<OinkProvider><MicrophoneInput/></OinkProvider>);
 expect(screen.getByRole('combobox').textContent).toContain(label);
 expect(sectionsFor('mac').find(s=>s.id==='audio')?.items.at(-1)?.id).toBe('microphoneInput');
 const found=searchSections('mac',keyword).get('audio');
 expect(found==='all'||found?.has('microphoneInput')).toBe(true);
});
