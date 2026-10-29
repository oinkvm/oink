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
    fireEvent.click(screen.getAllByRole('switch')[0]);
    expect(container.querySelector('[data-key="caps"] .kmap__to')?.textContent).toBe('Caps Lock / Caps Lock');
  });
});

describe('custom None and Fn choices', () => {
  it('keeps a custom Caps/Fn choice even when no key menu was edited', () => {
    setLanguage('en');
    const { container } = render(<Mapping observed={false} />);
    fireEvent.click(screen.getByRole('radio', { name: 'Custom' }));
    fireEvent.click(screen.getAllByRole('switch')[0]);
    fireEvent.click(screen.getByRole('switch', { name: 'Allow mapping the Fn key' }));
    fireEvent.click(screen.getByRole('radio', { name: 'Windows feel' }));
    fireEvent.click(screen.getByRole('radio', { name: 'Custom' }));
    expect(container.querySelector('[data-key="caps"] .kmap__to')?.textContent).toBe('Caps Lock / Caps Lock');
    expect(container.querySelector('button[data-key="fn"]')).toBeTruthy();
  });
  it.each([
    ['zh-Hans', '允许为短按和长按 Caps Lock 映射不同功能'],
    ['en', 'Map different functions to short and long Caps Lock presses'],
    ['ja', 'Caps Lock の短押しと長押しに異なる機能を割り当てられます'],
    ['ko', 'Caps Lock을 짧게 누를 때와 길게 누를 때 서로 다른 기능을 매핑할 수 있습니다'],
  ] as const)('presents the Caps description in a standard settings row in %s', (language, description) => {
    setLanguage(language);
    const { container } = render(<Mapping observed={false} initial={{...DEFAULT_PREFS,keyPreset:'custom'}} />);
    const rows = container.querySelectorAll('.kmap__options > .ok-row');
    expect(rows).toHaveLength(2);
    expect(rows[0].querySelector('.ok-row__desc')?.textContent).toBe(description);
    expect(rows[1].querySelector('.ok-row__label')).toBeTruthy();
    expect(rows[1].querySelector('.ok-row__desc')?.textContent).toContain('Fn /');
    expect(rows[1].querySelector('[role="switch"]')).toBeTruthy();
  });
  it('only exposes the Fn key when enabled and offers None and sided modifiers', () => {
    setLanguage('en');
    const { container } = render(<Mapping observed={false} initial={{...DEFAULT_PREFS,keyPreset:'custom'}} />);
    expect(container.querySelector('button[data-key="fn"]')).toBeNull();
    fireEvent.click(screen.getByRole('switch', { name: 'Allow mapping the Fn key' }));
    fireEvent.click(container.querySelector('button[data-key="fn"]')!);
    expect(screen.getByRole('menuitemradio', { name: 'None' })).toBeTruthy();
    expect(screen.getByRole('menuitemradio', { name: 'Right Ctrl' })).toBeTruthy();
    fireEvent.click(screen.getByRole('menuitemradio', { name: 'None' }));
    expect(container.querySelector('[data-key="fn"] .kmap__to')?.textContent).toBe('None');
  });
});
