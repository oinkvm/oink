// @vitest-environment jsdom
import { useLayoutEffect } from 'react';
import { cleanup, render } from '@testing-library/react';
import { afterEach, beforeAll, expect, it } from 'vitest';
import { OinkProvider, useOink } from 'oink-ui';
import { AppearanceSync } from './AppearanceSync';
import { DEFAULT_ACCENT } from './appearance';
import type { LocalPreferences, PresentationPreferences } from './model';

beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { configurable: true, value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
afterEach(cleanup);
const presentation = { motion: 'reduced' } as PresentationPreferences;

it('starts pink before preferences arrive, then honours explicit blue and custom mode colours', () => {
  const seen: string[] = [];
  function Probe() {
    const { accent } = useOink();
    useLayoutEffect(() => { seen.push(accent); }, [accent]);
    return null;
  }
  const view = (local: LocalPreferences | null) => <OinkProvider defaultAccent={DEFAULT_ACCENT.value}>
    <AppearanceSync local={local} presentation={presentation}/><Probe/>
  </OinkProvider>;
  const { rerender } = render(view(null));
  expect(seen).toEqual(['pink']);
  expect(document.documentElement.dataset.accent).toBe('pink');
  const saved = { theme_mode: 'light', theme_accent: { light: '#2E7BFF', dark: '#2E7BFF' } } as LocalPreferences;
  rerender(view(saved));
  expect(seen.at(-1)).toBe('blue');
  rerender(view({ ...saved, theme_mode: 'dark', theme_accent: { light: '#123456', dark: '#ABCDEF' } }));
  expect(seen.at(-1)).toBe('#abcdef');
});
