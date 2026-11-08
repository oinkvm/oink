import { describe, it, expect } from 'vitest';
import { sectionsFor, text } from './schema';
import { setLanguage } from '../i18n';
import { DEFAULT_PREFS } from './prefs';
describe('Paste as File',()=>{
 it('places the local tool after shortcuts and describes the native file manager',()=>{
  setLanguage('zh-Hans');
  for(const platform of ['mac','win'] as const){
   const sections=sectionsFor(platform), index=sections.findIndex(s=>s.id==='shortcuts');
   expect(sections[index+1].id).toBe('tools');
   const row=sections[index+1].items.find(i=>i.id==='pasteAsFile')!;
   expect(text(row.desc,platform)).toContain(platform==='mac'?'Finder':'资源管理器');
  }
 });
 it('enables the tool by default',()=>expect(DEFAULT_PREFS.pasteAsFile).toBe(true));
});
