// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { DEFAULT_SHORTCUTS, SHORTCUTS, ShortcutRecorder } from './Shortcuts';
import { setLanguage } from '../i18n';
beforeAll(()=>{Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})});});
afterEach(()=>{cleanup();setLanguage('en');});
it('places enabled single-key escape immediately after the lock toggle',()=>{
  expect(SHORTCUTS[SHORTCUTS.findIndex(s=>s.id==='lock')+1].id).toBe('lockEscape');
  expect(DEFAULT_SHORTCUTS.lockEscape).toEqual({on:true,keys:'Alt'});
  setLanguage('en');
  expect(SHORTCUTS.find(s=>s.id==='lock')?.label).toBe('Lock/Unlock pointer to current device');
});
it.each([{key:'Meta',code:'MetaRight',metaKey:true}, {key:'x',code:'KeyX'}])('records a single key %s',event=>{
  const onChange=vi.fn();
  render(<OinkProvider><ShortcutRecorder platform="mac" label="escape" value="Meta" singleKey onChange={onChange} onReject={vi.fn()}/></OinkProvider>);
  const recorder=screen.getByRole('button'); fireEvent.click(recorder); fireEvent.keyDown(recorder,event);
  expect(onChange).toHaveBeenCalledExactlyOnceWith(event.key==='Meta'?'Meta':'X');
});
it('commits the first modifier immediately and ignores the next key',()=>{
  const onChange=vi.fn(),onReject=vi.fn();
  render(<OinkProvider><ShortcutRecorder platform="win" label="escape" value="Meta" singleKey onChange={onChange} onReject={onReject}/></OinkProvider>);
  const recorder=screen.getByRole('button');fireEvent.click(recorder);
  fireEvent.keyDown(recorder,{key:'Control',code:'ControlLeft',ctrlKey:true});
  fireEvent.keyDown(recorder,{key:'x',code:'KeyX',ctrlKey:true});
  fireEvent.keyUp(recorder,{key:'x',code:'KeyX',ctrlKey:true});
  expect(onChange).toHaveBeenCalledExactlyOnceWith('Control');expect(onReject).not.toHaveBeenCalled();
});

it.each([
  ['zh-Hans','锁定/解锁指针到当前设备'], ['en','Lock/Unlock pointer to current device'],
  ['ja','ポインタを現在のデバイスにロック/解除'], ['ko','포인터를 현재 기기에 고정/해제'],
] as const)('uses toggle copy in %s', (language,label)=>{
  setLanguage(language); expect(SHORTCUTS.find(s=>s.id==='lock')?.label).toBe(label);
});
it.each([
  [{key:'x',code:'KeyX'},{key:'y',code:'KeyY'}],
  [{key:'Meta',code:'MetaLeft',metaKey:true},{key:'Shift',code:'ShiftLeft',metaKey:true,shiftKey:true}],
])('finishes before a second key can join the recording', (first,second)=>{
  const onChange=vi.fn(),onReject=vi.fn();
  render(<OinkProvider><ShortcutRecorder platform="mac" label="escape" value="Meta" singleKey onChange={onChange} onReject={onReject}/></OinkProvider>);
  const recorder=screen.getByRole('button');fireEvent.click(recorder);
  fireEvent.keyDown(recorder,first);fireEvent.keyDown(recorder,second);fireEvent.keyUp(recorder,second);
  expect(onChange).toHaveBeenCalledExactlyOnceWith(first.key==='x'?'X':'Meta');expect(onReject).not.toHaveBeenCalled();
});
it('records punctuation using the backend key vocabulary',()=>{
  const onChange=vi.fn();
  render(<OinkProvider><ShortcutRecorder platform="mac" label="escape" value="Meta" singleKey onChange={onChange} onReject={vi.fn()}/></OinkProvider>);
  const recorder=screen.getByRole('button');fireEvent.click(recorder);
  fireEvent.keyDown(recorder,{key:';',code:'Semicolon'});fireEvent.keyUp(recorder,{key:';',code:'Semicolon'});
  expect(onChange).toHaveBeenCalledExactlyOnceWith('Semicolon');
});

it('starts recording with the keyboard without saving the activation key',()=>{
  const onChange=vi.fn();
  render(<OinkProvider><ShortcutRecorder platform="mac" label="escape" value="Meta" singleKey onChange={onChange} onReject={vi.fn()}/></OinkProvider>);
  const recorder=screen.getByRole('button');
  fireEvent.keyDown(recorder,{key:'Enter',code:'Enter'});fireEvent.keyUp(recorder,{key:'Enter',code:'Enter'});
  expect(onChange).not.toHaveBeenCalled();expect(recorder.getAttribute('aria-pressed')).toBe('true');
  fireEvent.keyDown(recorder,{key:'Meta',code:'MetaLeft',metaKey:true});fireEvent.keyUp(recorder,{key:'Meta',code:'MetaLeft'});
  expect(onChange).toHaveBeenCalledExactlyOnceWith('Meta');
});

it('does not restart recording on repeats of a recorded activation key',()=>{
  const onChange=vi.fn();
  render(<OinkProvider><ShortcutRecorder platform="mac" label="escape" value="Meta" singleKey onChange={onChange} onReject={vi.fn()}/></OinkProvider>);
  const recorder=screen.getByRole('button');fireEvent.click(recorder);
  fireEvent.keyDown(recorder,{key:'Enter',code:'Enter'});
  expect(onChange).toHaveBeenCalledExactlyOnceWith('Enter');
  fireEvent.keyDown(recorder,{key:'Enter',code:'Enter',repeat:true});
  expect(recorder.getAttribute('aria-pressed')).toBe('false');
});

it.each(['mac','win'] as const)('offers a default-off shared microphone shortcut on %s',platform=>{
 setLanguage('en');
 expect(DEFAULT_SHORTCUTS.microphoneMute).toEqual({on:false,keys:'Control+Alt+M'});
 const row=SHORTCUTS.find(s=>s.id==='microphoneMute')!;
 expect(row.label).toBe('Mute shared microphone');
 render(<OinkProvider><ShortcutRecorder platform={platform} label={row.label} value={DEFAULT_SHORTCUTS.microphoneMute.keys} onChange={vi.fn()} onReject={vi.fn()}/></OinkProvider>);
 expect(screen.getByRole('button').textContent).toContain('M');
});
