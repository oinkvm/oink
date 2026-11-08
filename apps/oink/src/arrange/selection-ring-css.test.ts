import { readFileSync } from 'node:fs';
import { expect, it } from 'vitest';

it('keeps selection rings on the shared shape and contrast tokens', () => {
  const css = readFileSync(new URL('../app.css', import.meta.url), 'utf8');
  for (const selector of ['.scr__selection', '.scr__group-border']) {
    const rule = css.slice(css.indexOf(selector + ' {')).split('}')[0];
    expect(rule).toMatch(/corner-shape:\s*var\(--ok-corner\)/);
    expect(rule).toMatch(/--ok-shape:\s*var\(--ok-corner\)/);
    expect(rule).toMatch(/box-shadow:[^;]*var\(--ok-well\)[^;]*var\(--ok-accent\)/);
  }
});
