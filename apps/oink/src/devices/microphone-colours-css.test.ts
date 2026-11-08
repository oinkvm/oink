import { readFileSync } from 'node:fs';
import { expect, it } from 'vitest';

it('keeps microphone tones and badges on the shared CSS tokens', () => {
  const css = readFileSync(new URL('./devices.css', import.meta.url), 'utf8');
  expect(css).toMatch(/\.dev-ico\[data-tone="accent"\]\s*\{\s*color: var\(--ok-accent\)/);
  expect(css).toMatch(/\.dev-badge\[data-badge="send"\]\s*\{\s*color: inherit;/);
  expect(css).toMatch(/\.vol\[data-tone="accent"\]\s*\{\s*--vol-c: var\(--ok-accent\); --vol-on: var\(--ok-on-accent\)/);
  expect(css).toContain('[data-accent="pink"] .vol[data-tone="accent"] { --vol-on: #FFFFFF; }');
  expect(css).toMatch(/\.vol__meter \.ok-meter__fill\s*\{\s*background: var\(--vol-c\)/);
});
