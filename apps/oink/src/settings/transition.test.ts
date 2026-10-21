import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

const source = readFileSync(new URL('./settings.css', import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');

function block(css: string, at: string): string {
  const start = css.indexOf(`${at} {`);
  if (start < 0) return '';
  let depth = 0;
  for (let i = css.indexOf('{', start); i < css.length; i++) {
    if (css[i] === '{') depth++;
    else if (css[i] === '}' && --depth === 0) return css.slice(css.indexOf('{', start) + 1, i);
  }
  throw new Error(`unclosed ${at}`);
}

function rules(css: string): Map<string, Record<string, string>> {
  const out = new Map<string, Record<string, string>>();
  for (const [, selector, body] of css.matchAll(/([^{}@]+)\{([^{}]*)\}/g)) {
    const decls: Record<string, string> = {};
    for (const d of body.split(';')) {
      const i = d.indexOf(':');
      if (i > 0) decls[d.slice(0, i).trim()] = d.slice(i + 1).trim();
    }
    out.set(selector.trim(), { ...out.get(selector.trim()), ...decls });
  }
  return out;
}

const startingStyle = block(source, '@starting-style');
const css = rules(startingStyle ? source.replace(`@starting-style {${startingStyle}}`, '') : source);
const rule = (selector: string) => {
  const r = css.get(selector);
  expect(r, selector).toBeDefined();
  return r!;
};
const timing = (transition: string, property: string) =>
  transition.split(/,(?![^(]*\))/).map(s => s.trim()).find(s => s.startsWith(`${property} `))?.slice(property.length + 1);

describe('settings page transition', () => {
  it('paints its own surface on one layer above the arrangement', () => {
    const page = rule('.setp');
    expect(page.background).toBe('var(--ok-surface)');
    expect(page.position).toBe('relative');
    expect(Number(page['z-index'])).toBeGreaterThan(0);
    // The canvas tools' z-index must not escape above the page.
    expect(rule('.disp').isolation).toBe('isolate');
  });

  it('leaves the arrangement static so only the page animates', () => {
    for (const selector of ['.disp', '.disp[data-hidden]']) {
      const r = rule(selector);
      expect(r.opacity, selector).toBeUndefined();
      expect(timing(r.transition, 'opacity'), selector).toBeUndefined();
    }
    // The arrangement stays visible under the page until the entry has finished.
    expect(timing(rule('.disp[data-hidden]').transition, 'visibility')).toBe(`0s linear ${timing(rule('.setp[data-open]').transition, 'opacity')!.split(' ')[0]}`);
  });

  it('moves the rows on the same timing as the page fade, in both directions', () => {
    expect(timing(rule('.setp[data-open] > .setp__inner').transition, 'transform')).toBe(timing(rule('.setp[data-open]').transition, 'opacity'));
    expect(timing(rule('.setp > .setp__inner').transition, 'transform')).toBe(timing(rule('.setp').transition, 'opacity'));
    expect(rule('.setp').transform).toBeUndefined();
  });

  it('starts the first open, which mounts already open, from the closed values', () => {
    const first = rules(startingStyle);
    expect(first.get('.setp[data-open]')?.opacity).toBe(rule('.setp').opacity);
    expect(first.get('.setp[data-open] > .setp__inner')?.transform).toBe(rule('.setp > .setp__inner').transform);
  });
});
