import type { en } from './messages/en';

/**
 * A count-dependent message. `plural` is the index of the value whose CLDR
 * plural category (for the active language) selects the form; `other` is
 * required because every language has it.
 */
export type PluralMessage = { readonly plural: number; readonly other: string }
  & { readonly [C in Exclude<Intl.LDMLPluralRule, 'other'>]?: string };
export type Message = string | PluralMessage;

/**
 * Message keys are the designer's Simplified Chinese copy (the source
 * language), with `{0}`, `{1}`… placeholders. A key may start with an ASCII
 * context and `|` (for example `state|关闭`) when the same Chinese text needs
 * different translations; the context is not displayed.
 *
 * The English catalog is the reference that defines the key set; every other
 * catalog must contain exactly the same keys.
 */
export type MessageKey = keyof typeof en;
export type Catalog = { readonly [K in MessageKey]: Message };

/** The Simplified Chinese text of a key: the key without its context. */
export const sourceText = (key: string) => key.replace(/^[a-z][a-z-]*\|/, '');
