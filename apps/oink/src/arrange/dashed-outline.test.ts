import { readFileSync } from 'node:fs';
import { expect, it } from 'vitest';

/** The library preserves native WebKit dashes while supported engines use squircle corners. */
it('pairs the intended corner shape on every dashed outline', () => {
  const css = readFileSync(new URL('../app.css', import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
  const dashed = [...css.matchAll(/([^{}]+)\{([^}]*\bdashed\b[^}]*)\}/g)];
  expect(dashed.length).toBeGreaterThan(0);
  for (const [, selector, body] of dashed) {
    expect(body, selector.trim()).toMatch(/--ok-shape:\s*var\(--ok-corner\)/);
    expect(body, selector.trim()).toMatch(/(^|;)\s*corner-shape:\s*var\(--ok-corner\)/);
  }
});
