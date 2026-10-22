// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { setLanguage } from '../i18n';
import type { Pairing } from './usePairing';
import { ManualSearch } from './PairDialog';

beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
});
afterEach(cleanup);

it('leaves the manual IP box without placeholder text', () => {
  setLanguage('en');
  const pairing = { actions: { search: vi.fn() }, defaultPort: 47470 } as unknown as Pairing;
  render(<OinkProvider defaultMotion="reduced"><ManualSearch pairing={pairing}/></OinkProvider>);
  expect(screen.getByLabelText(/^IP address/).hasAttribute('placeholder')).toBe(false);
});
