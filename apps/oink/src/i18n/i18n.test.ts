// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { CATALOGS, LANGUAGES, formatList, formatNumber, languageName, resolveLanguage, secondaryName, setLanguage, tr } from '.';
import type { Message, MessageKey } from '.';
import { sourceText } from './catalog';
import { en } from './messages/en';

afterEach(() => { delete window.__OINK_SYSTEM_LANGUAGES__; vi.restoreAllMocks(); setLanguage('en'); });

describe('automatic language', () => {
  it.each(['zh', 'zh-Hans', 'zh-Hans-CN', 'zh-CN', 'zh-SG', 'ZH_cn'])('maps %s to Simplified Chinese', system => {
    expect(resolveLanguage('auto', system)).toBe('zh-Hans');
  });
  it.each(['ja', 'ja-JP', 'JA_jp', 'ja-Jpan-JP'])('maps %s to Japanese', system => {
    expect(resolveLanguage('auto', system)).toBe('ja');
  });
  it.each(['ko', 'ko-KR', 'ko_KP', 'ko-Kore-KR'])('maps %s to Korean', system => {
    expect(resolveLanguage('auto', system)).toBe('ko');
  });
  it.each(['zh-Hant', 'zh-Hant-CN', 'zh-TW', 'zh-HK', 'zh-MO', 'zh-Hant-HK', 'en-US', 'fr', '', 'zhx', 'zh-XX', 'jav', 'kok'])('maps %s to English', system => {
    expect(resolveLanguage('auto', system)).toBe('en');
  });
  it('honors preferred-language order and explicit choices', () => {
    expect(resolveLanguage('auto', ['en-US', 'zh-CN'])).toBe('en');
    expect(resolveLanguage('auto', ['zh-CN', 'en-US'])).toBe('zh-Hans');
    expect(resolveLanguage('auto', ['ja-JP', 'en-US'])).toBe('ja');
    expect(resolveLanguage('auto', ['zh-Hant', 'zh-CN'])).toBe('en');
    expect(resolveLanguage('auto', [])).toBe('en');
    expect(resolveLanguage('en', ['zh-CN'])).toBe('en');
    expect(resolveLanguage('zh-Hans', ['en-US'])).toBe('zh-Hans');
    expect(resolveLanguage('ja', ['en-US'])).toBe('ja');
    expect(resolveLanguage('ko', ['ja-JP'])).toBe('ko');
  });
  it('uses the native macOS/Windows list even when the webview reports English', () => {
    vi.spyOn(navigator, 'language', 'get').mockReturnValue('en-US');
    vi.spyOn(navigator, 'languages', 'get').mockReturnValue(['en-US']);
    window.__OINK_SYSTEM_LANGUAGES__ = ['ko-KR', 'en-US'];
    setLanguage('auto');
    expect(document.documentElement.lang).toBe('ko');
    expect(tr('通用')).toBe('일반');
  });
  it('uses browser preferences when running without a native shell', () => {
    vi.spyOn(navigator, 'language', 'get').mockReturnValue('en-US');
    vi.spyOn(navigator, 'languages', 'get').mockReturnValue(['ja-JP', 'en-US']);
    setLanguage('auto');
    expect(document.documentElement.lang).toBe('ja');
  });
});

const placeholders = (text: string) => [...new Set(text.match(/\{\d+\}/g) ?? [])].sort();
const forms = (message: Message) => typeof message === 'string' ? [message] : Object.entries(message).filter(([k]) => k !== 'plural').map(([, v]) => v as string);
const keys = Object.keys(en) as MessageKey[];

describe('catalog completeness', () => {
  it.each(Object.keys(CATALOGS) as (keyof typeof CATALOGS)[])('%s has exactly the reference keys', language => {
    expect(Object.keys(CATALOGS[language]).sort()).toEqual([...keys].sort());
  });
  it.each(Object.keys(CATALOGS) as (keyof typeof CATALOGS)[])('%s keeps every placeholder and uses only its plural categories', language => {
    const categories = new Set(new Intl.PluralRules(language).resolvedOptions().pluralCategories);
    for (const key of keys) {
      const message: Message = CATALOGS[language][key];
      for (const form of forms(message)) {
        expect(form.trim(), `${language} ${key}`).not.toBe('');
        expect(placeholders(form), `${language} ${key}: ${form}`).toEqual(placeholders(sourceText(key)));
      }
      if (typeof message !== 'string') {
        expect(Number.isInteger(message.plural) && message.plural >= 0, `${language} ${key}`).toBe(true);
        for (const category of Object.keys(message).filter(k => k !== 'plural')) expect(categories.has(category as Intl.LDMLPluralRule), `${language} ${key}: ${category}`).toBe(true);
      }
    }
  });
  it('writes Korean in Hangul without particles that depend on an inserted word', () => {
    for (const key of keys) for (const form of forms(CATALOGS.ko[key])) {
      // The language setting's search keywords list every language's own name.
      if (key !== 'language 多语言 中文 英文 english') expect(form, key).not.toMatch(/\p{Script=Han}/u);
      expect(form, key).not.toMatch(/\{\d+\}(?:을|를|이|가|은|는|와|과|으로|로)(?![가-힣])/u);
    }
  });
  it('translates Japanese instead of reusing the Chinese source', () => {
    // Identical Han words in both languages.
    const same = new Set(['音量', '管理', '警告', '保存', '最小化', '最大化', '{0} 秒前', '（{0}）', '、']);
    for (const key of keys) {
      const message = CATALOGS.ja[key];
      if (typeof message === 'string' && /\p{Script=Han}/u.test(key) && !same.has(message)) expect(message, key).not.toBe(sourceText(key));
    }
  });
});

describe.each(['en', 'zh-Hans', 'ja', 'ko'] as const)('counted messages in %s', language => {
  it.each([0, 1, 2, 1.5])('agrees with a count of %s', count => {
    setLanguage(language);
    const plural = count === 1 ? '' : 's';
    const expected = {
      en: [`Latency ${count} millisecond${plural}`, `${count} second${plural} ago`, `minute${plural}`, `${count} operation${plural} ${count === 1 ? 'is' : 'are'} still pending and will be interrupted.`],
      'zh-Hans': [`延迟 ${count} 毫秒`, `${count} 秒前`, '分钟', `还有 ${count} 项操作未完成，退出会中断它们。`],
      ja: [`遅延 ${count} ミリ秒`, `${count} 秒前`, '分', `完了していない操作が ${count} 件あります。終了すると中断されます。`],
      ko: [`지연 시간 ${count}밀리초`, `${count}초 전`, '분', `완료되지 않은 작업이 ${count}개 있습니다. 종료하면 중단됩니다.`],
    }[language];
    expect([tr('延迟 {0} 毫秒', count), tr('{0} 秒前', count), tr('分钟', count), tr('还有 {0} 项操作未完成，退出会中断它们。', count)]).toEqual(expected);
  });
  it('keeps measurement symbols invariant', () => {
    setLanguage(language);
    for (const count of [0, 1, 2]) expect(tr('{0}，在线，延迟 {1} ms', 'Remote', count)).toMatch(new RegExp(`Remote.*${count} ?ms$`));
  });
  it('shows the context-free source text and its translation', () => {
    setLanguage(language);
    expect(tr('state|关闭')).toBe({ en: 'Off', 'zh-Hans': '关闭', ja: 'オフ', ko: '끔' }[language]);
  });
});

it('does not interpret placeholders inside interpolated device names', () => {
  setLanguage('en');
  expect(tr('{0} {1} 毫秒', '{1}', 1)).toBe('{1} 1 millisecond');
});

it('formats numbers, lists and language names for the active language', () => {
  setLanguage('en');
  expect(formatNumber(1234.5, { maximumFractionDigits: 1 })).toBe('1,234.5');
  expect(formatList(['A', 'B', 'C'])).toBe('A, B, and C');
  expect(languageName('ja')).toBe('Japanese');
  setLanguage('ja');
  expect(formatList(['A', 'B', 'C'])).toBe('A、B、C');
  expect(languageName('ko')).toBe('韓国語');
});

it('lists every language under its own name, with a secondary name only when it differs', () => {
  expect(LANGUAGES.map(l => l.autonym)).toEqual(['简体中文', 'English', '日本語', '한국어']);
  setLanguage('ko');
  expect(LANGUAGES.map(secondaryName)).toEqual(['중국어(간체)', '영어', '일본어', undefined]);
});
