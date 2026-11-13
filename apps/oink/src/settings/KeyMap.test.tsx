// @vitest-environment jsdom
import { useState } from 'react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { KeyMapControl } from './KeyMap';
import { DEFAULT_PREFS, type KeyTarget, type Prefs, type PrefsApi } from './prefs';
import { setLanguage } from '../i18n';

beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: true, addEventListener() {}, removeEventListener() {} }) });
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  Element.prototype.scrollIntoView = () => {};
});
afterEach(cleanup);
function Mapping({ observed, initial, platform = 'mac', target, onTarget }: { observed: boolean | null; initial?: Prefs; platform?: 'mac' | 'win'; target?: KeyTarget; onTarget?: (target: KeyTarget) => void }) {
  const [prefs, setPrefs] = useState(initial ?? { ...DEFAULT_PREFS, keyPreset: 'mac' });
  const [presentation, setPresentation] = useState({ motion: 'reduced' as const, hide_sponsor: false, compact_layout: true, keymap_target: target });
  const savePresentation = (patch: Partial<typeof presentation>) => { if (patch.keymap_target) onTarget?.(patch.keymap_target); setPresentation(p => ({ ...p, ...patch })); };
  return <OinkProvider defaultMotion="reduced"><KeyMapControl platform={platform} api={{ prefs, setPrefs, presentation, savePresentation } as unknown as PrefsApi} capsIme={observed} label="Key mapping" onLog={() => {}} /></OinkProvider>;
}
function choose(label: string, option: string) {
  fireEvent.click(within(screen.getByRole('radiogroup', { name: label })).getByRole('radio', { name: option }));
}
const options = (label: string) => within(screen.getByRole('radiogroup', { name: label })).getAllByRole('radio').map(o => o.textContent);
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
    choose('Key mapping: preset', 'Custom');
    expect(container.querySelector('[data-key="caps"] .kmap__to')?.textContent).toBe('Caps Lock');
    fireEvent.click(screen.getAllByRole('switch')[0]);
    expect(container.querySelector('[data-key="caps"] .kmap__to')?.textContent).toBe('Caps Lock / Caps Lock');
  });
});

describe('custom None and Fn choices', () => {
  it('keeps a custom Caps/Fn choice even when no key menu was edited', () => {
    setLanguage('en');
    const { container } = render(<Mapping observed={false} />);
    choose('Key mapping: preset', 'Custom');
    fireEvent.click(screen.getAllByRole('switch')[0]);
    fireEvent.click(screen.getByRole('switch', { name: 'Allow mapping the Fn key' }));
    choose('Key mapping: preset', 'Windows feel');
    choose('Key mapping: preset', 'Custom');
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

describe('target system and feel selectors', () => {
  it.each([
    ['zh-Hans', '目标系统', 'Key mapping：预设', ['macOS 手感', 'Android 手感', '自定义']],
    ['en', 'Target system', 'Key mapping: preset', ['macOS feel', 'Android feel', 'Custom']],
    ['ja', '対象システム', 'Key mapping：プリセット', ['macOS の操作感', 'Android の操作感', 'カスタム']],
    ['ko', '대상 시스템', 'Key mapping: 프리셋', ['macOS 감각', 'Android 감각', '사용자 지정']],
  ] as const)('keeps the segmented labels and selected phone preset in %s', (language, targetLabel, presetLabel, names) => {
    setLanguage(language);
    render(<Mapping observed={false} target="android" initial={{ ...DEFAULT_PREFS, keyPreset: 'android' }} />);
    expect(options(targetLabel)).toEqual(['macOS', 'Windows', 'Android']);
    expect(options(presetLabel)).toEqual(names);
    expect(within(screen.getByRole('radiogroup', { name: targetLabel })).getByRole('radio', { name: 'Android', checked: true })).toBeTruthy();
    expect(within(screen.getByRole('radiogroup', { name: presetLabel })).getByRole('radio', { name: names[1], checked: true })).toBeTruthy();
  });
  it('names each feel after a real system and dedupes a same-system target', () => {
    setLanguage('en');
    render(<Mapping observed={false} />);
    expect(screen.getAllByRole('radiogroup')).toHaveLength(2);
    expect(screen.queryByRole('combobox')).toBeNull();
    expect(options('Target system')).toEqual(['macOS', 'Windows', 'Android']);
    expect(options('Key mapping: preset')).toEqual(['macOS feel', 'Windows feel', 'Custom']);
    choose('Target system', 'Android');
    expect(options('Key mapping: preset')).toEqual(['macOS feel', 'Android feel', 'Custom']);
    choose('Key mapping: preset', 'Android feel');
    choose('Target system', 'macOS');
    expect(options('Key mapping: preset')).toEqual(['macOS feel', 'Custom']);
    expect(within(screen.getByRole('radiogroup', { name: 'Key mapping: preset' })).getByRole('radio', { name: 'macOS feel', checked: true })).toBeTruthy();
    choose('Target system', 'Android');
    expect(within(screen.getByRole('radiogroup', { name: 'Key mapping: preset' })).getByRole('radio', { name: 'Android feel', checked: true })).toBeTruthy();
  });
  it('saves the target choice and shows the Android key names', () => {
    setLanguage('en');
    const saved: KeyTarget[] = [];
    const { container } = render(<Mapping observed={false} platform="win" initial={{ ...DEFAULT_PREFS, keyPreset: 'android' }} target="android" onTarget={t => saved.push(t)} />);
    expect(within(screen.getByRole('radiogroup', { name: 'Target system' })).getByRole('radio', { name: 'Android', checked: true })).toBeTruthy();
    expect(container.querySelector('[data-key="win"] .kmap__to')?.textContent).toBe('Meta');
    choose('Target system', 'macOS');
    expect(saved).toEqual(['macos']);
  });
});
