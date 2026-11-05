// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
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

it('accepts an explicit IP:port and sends the declared remote port', async () => {
  setLanguage('en');
  const search=vi.fn().mockRejectedValue(new Error('probe finished'));
  const pairing={actions:{search},defaultPort:47470} as unknown as Pairing;
  render(<OinkProvider defaultMotion="reduced"><ManualSearch pairing={pairing}/></OinkProvider>);
  fireEvent.change(screen.getByLabelText(/^IP address/),{target:{value:'192.0.2.20:54321'}});
  fireEvent.click(screen.getByRole('button',{name:'Search'}));
  await waitFor(()=>expect(search).toHaveBeenCalledWith('192.0.2.20',54321));
});
