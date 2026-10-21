// @vitest-environment jsdom
import { useState } from 'react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { KeyMapControl } from './KeyMap';
import { DEFAULT_PREFS, type Prefs, type PrefsApi } from './prefs';
import { setLanguage } from '../i18n';

beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: true, addEventListener() {}, removeEventListener() {} }) });
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
});
afterEach(cleanup);
function Mapping({ observed, initial }: { observed: boolean | null; initial?: Prefs }) {
  const [prefs, setPrefs] = useState(initial ?? { ...DEFAULT_PREFS, keyPreset: 'mac' });
  return <OinkProvider defaultMotion="reduced"><KeyMapControl platform="mac" api={{ prefs, setPrefs } as PrefsApi} capsIme={observed} label="Windows key mapping" onLog={() => {}} /></OinkProvider>;
}
describe('native Caps mapping presentation', () => {
  it.each(['en', 'zh-Hans'] as const)('shows Caps Lock for an unavailable observation in %s', language => {
    setLanguage(language);
    const { container } = render(<Mapping observed={null} />);
    expect(container.querySelector('[data-key="caps"] .kmap__to')?.textContent).toBe('Caps Lock');
    expect(container.textContent).not.toMatch(/unavailable|无法读取|—/);
    expect(container.querySelector('.kmap__note')?.textContent).toContain('Caps Lock');
  });
  it.each([false, true])('follows the observed system preference %s', observed => {
    setLanguage('en');
    const { container } = render(<Mapping observed={observed} />);
    expect(container.querySelector('[data-key="caps"] .kmap__to')?.textContent).toBe(observed ? 'Shift / Caps Lock' : 'Caps Lock');
    expect(!!container.querySelector('.kmap__note')).toBe(!observed);
  });
  it('seeds custom tap and hold from the displayed fallback, including stale custom data', () => {
    setLanguage('en');
    const initial = { ...DEFAULT_PREFS, keyPreset: 'mac' as const, keyCustom: { ...DEFAULT_PREFS.keyCustom, mac: { keys: { caps: 'shift' }, capsLong: 'shift' } } };
    const { container } = render(<Mapping observed={null} initial={initial} />);
    fireEvent.click(screen.getByRole('radio', { name: 'Custom' }));
    expect(container.querySelector('[data-key="caps"] .kmap__to')?.textContent).toBe('Caps Lock');
    fireEvent.click(screen.getByRole('switch'));
    expect(container.querySelector('[data-key="caps"] .kmap__to')?.textContent).toBe('Caps Lock / Caps Lock');
  });
});
