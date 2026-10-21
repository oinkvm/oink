import { readFileSync } from 'node:fs';
import { expect, it } from 'vitest';

/**
 * On WebKit (no corner-shape), oink-ui's corner fallback repaints the outline of any
 * element whose --ok-shape is not round as a border-image patch next to the native
 * outline. The patch has no dash style, so a dashed outline on such an element shows
 * twice. Every dashed rule must therefore keep round corners.
 */
it('keeps every dashed outline out of the WebKit corner fallback', () => {
  const css = readFileSync(new URL('../app.css', import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
  const dashed = [...css.matchAll(/([^{}]+)\{([^}]*\bdashed\b[^}]*)\}/g)];
  expect(dashed.length).toBeGreaterThan(0);
  for (const [, selector, body] of dashed) {
    expect(body, selector.trim()).toMatch(/--ok-shape:\s*round\b/);
    expect(body, selector.trim()).toMatch(/(^|;)\s*corner-shape:\s*round\b/);
  }
});
