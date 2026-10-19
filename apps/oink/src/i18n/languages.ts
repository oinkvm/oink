/** Interface languages, in the order the language menus list them. */
export const LANGUAGES = [
  { id: 'zh-Hans', autonym: '简体中文' },
  { id: 'en', autonym: 'English' },
  { id: 'ja', autonym: '日本語' },
  { id: 'ko', autonym: '한국어' },
] as const;
export type Language = typeof LANGUAGES[number]['id'];
/** The persisted preference: a language, or `auto` to follow the system. */
export type LanguageChoice = Language | 'auto';

const isLanguage = (value: string): value is Language => LANGUAGES.some(l => l.id === value);

/**
 * Map the preference and the system's first preferred language to a UI language.
 *
 * `auto` uses ja for any Japanese tag, ko for any Korean tag and Simplified
 * Chinese for zh, zh-Hans, zh-CN and zh-SG. Traditional Chinese (zh-Hant,
 * zh-TW, zh-HK, zh-MO) falls back to English like every other language: the
 * operating systems do not substitute Simplified for Traditional when they pick
 * our native resources (permission prompts, menus), and Simplified is a
 * different script with different regional vocabulary, not a fallback that
 * Traditional Chinese readers expect.
 */
export function resolveLanguage(choice: string, system: string | readonly string[]): Language {
  if (choice !== 'auto') return isLanguage(choice) ? choice : 'en';
  const [base, qualifier] = (typeof system === 'string' ? system : system[0] ?? '').toLowerCase().split(/[-_]/);
  if (base === 'ja' || base === 'ko') return base;
  return base === 'zh' && (qualifier === undefined || ['hans', 'cn', 'sg'].includes(qualifier)) ? 'zh-Hans' : 'en';
}
