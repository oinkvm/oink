import { createElement, Fragment, useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';
import { sourceText } from './catalog';
import type { Catalog, Message, MessageKey } from './catalog';
import { en } from './messages/en';
import { ja } from './messages/ja';
import { ko } from './messages/ko';
import { resolveLanguage } from './languages';
import type { Language, LanguageChoice } from './languages';

export { LANGUAGES, resolveLanguage } from './languages';
export type { Language, LanguageChoice } from './languages';
export type { Catalog, Message, MessageKey, PluralMessage } from './catalog';
export { problemText } from './problems';

/** Translations of the Simplified Chinese source keys. */
export const CATALOGS: { readonly [L in Exclude<Language, 'zh-Hans'>]: Catalog } = { en, ja, ko };

declare global {
  interface Window { __OINK_SYSTEM_LANGUAGES__?: readonly string[] }
}
function systemLanguages(): readonly string[] {
  // WebKit/WebView2 language preferences can describe the app/browser rather
  // than the user's UI language. The native bootstrap runs before this module.
  const native = typeof window === 'undefined' ? undefined : window.__OINK_SYSTEM_LANGUAGES__;
  return native ?? navigator.languages ?? [navigator.language];
}

let language: Language = 'en';
let plurals = new Intl.PluralRules(language);
const listeners = new Set<() => void>();
function apply(next: Language) {
  language = next;
  plurals = new Intl.PluralRules(next);
  // The document language selects locale-specific CJK glyphs and font stacks.
  if (typeof document !== 'undefined') document.documentElement.lang = next;
}
apply(resolveLanguage('auto', systemLanguages()));

export function setLanguage(choice: LanguageChoice | string) {
  apply(resolveLanguage(choice, systemLanguages()));
  listeners.forEach(l => l());
}
export function currentLanguage(): Language { return language; }
export function useLanguage(): Language {
  return useSyncExternalStore(l => { listeners.add(l); return () => { listeners.delete(l); }; }, () => language);
}

function message(key: MessageKey, values: readonly unknown[]): string {
  const entry: Message = language === 'zh-Hans' ? sourceText(key) : CATALOGS[language][key];
  if (typeof entry === 'string') return entry;
  return entry[plurals.select(Number(values[entry.plural]))] ?? entry.other;
}

/** Translate a message, replacing `{n}` with the n-th value. */
export function tr(key: MessageKey, ...values: unknown[]): string {
  return message(key, values).replace(/\{(\d+)\}/g, (_, index: string) => String(values[Number(index)] ?? ''));
}
/** Translate a complete sentence while retaining inline elements as elements. */
export function trNodes(key: MessageKey, ...values: ReactNode[]): ReactNode[] {
  return message(key, values).split(/\{(\d+)\}/g).map((part, index) =>
    index % 2 ? createElement(Fragment, { key: index }, values[Number(part)]) : part);
}

/** Locale-aware number formatting for the active language. */
export function formatNumber(value: number, options?: Intl.NumberFormatOptions): string {
  return new Intl.NumberFormat(language, options).format(value);
}
/** A language's name in the active language, e.g. 日本語 → "Japanese" in English. */
export function languageName(id: Language): string {
  return new Intl.DisplayNames(language, { type: 'language' }).of(id)!;
}
/** The secondary name a language menu shows next to an autonym, omitted when both are the same. */
export function secondaryName(entry: { id: Language; autonym: string }): string | undefined {
  const name = languageName(entry.id);
  return name === entry.autonym ? undefined : name;
}
/** Join items as a list in the active language ("A, B and C", "A、B、C"). */
export function formatList(items: readonly string[]): string {
  return new Intl.ListFormat(language, { type: 'conjunction' }).format(items);
}
