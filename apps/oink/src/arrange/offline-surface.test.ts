import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { describe, expect, it } from 'vitest';

const strip = (css: string) => css.replace(/\/\*[\s\S]*?\*\//g, '');
const app = strip(readFileSync(new URL('../app.css', import.meta.url), 'utf8'));
const kit = strip(readFileSync(createRequire(import.meta.url).resolve('oink-ui/styles.css'), 'utf8'));

function declarations(css: string, selector: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [, sel, body] of css.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    if (sel.trim() !== selector) continue;
    for (const d of body.split(';')) {
      const i = d.indexOf(':');
      if (i > 0) out[d.slice(0, i).trim()] = d.slice(i + 1).trim();
    }
  }
  return out;
}

const darkKit = declarations(kit, ':root[data-theme=dark]');
const resolveDark = (value: string): string => {
  const ref = /^var\((--[\w-]+)\)$/.exec(value);
  return ref ? resolveDark(darkKit[ref[1]]) : value;
};
const luminance = (hex: string) => {
  const [r, g, b] = [1, 3, 5].map(i => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map(c => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
};
const contrast = (a: string, b: string) => {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
};

describe('offline screen surface', () => {
  it('paints offline tiles from the surface tokens', () => {
    expect(declarations(app, '.scr__body[data-offline]')['background-color']).toBe('var(--scr-offline-bg)');
    expect(declarations(app, '.scr__offline').color).toBe('var(--scr-offline-ink)');
  });

  it('keeps the light near-black surface', () => {
    const light = declarations(app, '.scr__body');
    expect(light['--scr-offline-bg']).toBe('#171717');
    expect(light['--scr-offline-ink']).toBe('#7B7B7B');
  });

  it('fills dark offline tiles with the interface background, off the canvas', () => {
    const dark = declarations(app, ':root[data-theme="dark"] .scr__body');
    // The interface background is the main window surface.
    expect(dark['--scr-offline-bg']).toBe(declarations(app, '.win, .win-shadow__shape').background);
    expect(dark['--scr-offline-bg']).toMatch(/^var\(--ok-(?!accent)[\w-]+\)$/);
    const fill = resolveDark(dark['--scr-offline-bg']);
    const ink = resolveDark(dark['--scr-offline-ink']);
    // The arrangement canvas is --ok-well; the tile must not share its colour.
    expect(contrast(fill, resolveDark('var(--ok-well)'))).toBeGreaterThanOrEqual(1.1);
    // The glyph stays legible on the fill.
    expect(contrast(ink, fill)).toBeGreaterThanOrEqual(3);
    // The computer-name pill keeps its themed text contrast.
    expect(contrast(resolveDark('var(--ok-text)'), resolveDark('var(--ok-raised)'))).toBeGreaterThanOrEqual(7);
  });
});
