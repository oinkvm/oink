// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { AboutWindow } from './AboutWindow';
import { readFileSync } from 'node:fs';
const appCss = readFileSync('src/app.css', 'utf8');

const attribution = vi.hoisted(() => vi.fn());
vi.mock('../api/client', () => ({ shell: { attribution, openUrl: vi.fn() } }));
vi.mock('../api/transport', () => ({ inTauri: true }));
const expression = '(Apache-2.0 WITH LLVM-exception) AND (MIT OR BSD-3-Clause)';
const component = { id: 'test:library', name: 'Library', version: '1.0', ecosystem: 'npm', repository: null, licenseExpression: expression, selectedLicense: 'MIT', copyright: [], licenseFiles: [{ path: 'licenses/test.txt', sha256: '0'.repeat(64) }], scope: ['runtime'] };
beforeAll(() => {
  vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
  const style = document.createElement('style'); style.textContent = appCss; document.head.append(style);
  attribution.mockImplementation(async (path: string) => path === 'third-party-licenses.json' ? JSON.stringify({ schemaVersion: 1, components: [component] }) : 'License text');
});
afterEach(cleanup);
function about(platform: 'mac' | 'win' = 'mac') {
  return render(<OinkProvider><div className="oink-app" data-view="about"><AboutWindow platform={platform} open front version="0.1.0-beta.0123abcd" build="17" stage={{ w: 360, h: 500 }} onFront={() => {}} onClose={() => {}} /></div></OinkProvider>);
}
describe('About native content', () => {
  it('shows the entire commit-labelled version and numeric build', () => {
    about();
    expect(screen.getByText(/0\.1\.0-beta\.0123abcd \(17\)/)).toBeTruthy();
  });
  it.each(['mac', 'win'] as const)('fills the %s native frame without a rounded content clip', platform => {
    const { container } = about(platform);
    const root = container.querySelector<HTMLElement>('.win.about')!;
    expect(getComputedStyle(root).borderRadius).toBe('0');
    expect(getComputedStyle(root).boxShadow).toBe('none');
    if (platform === 'win') expect(screen.queryByRole('button', { name: /Close|关闭/ })).toBeNull();
  });
  it('preserves the separate rounded menu panel', () => {
    const { container } = render(<div className="oink-app" data-view="panel"><div className="native-panel" /></div>);
    expect(getComputedStyle(container.querySelector('.native-panel')!).borderRadius).toContain('18px');
  });
  it('shows the illustration only on home, including after returning from either license page', async () => {
    const { container } = about();
    expect(container.querySelector('.about__doodle')).not.toBeNull();
    expect(screen.queryByRole('button', { name: /Privacy policy|隐私政策/ })).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: /Open source attributions|开源软件引用/ }));
    const library = await screen.findByRole('button', { name: /Library/ });
    expect(container.querySelector('.about__doodle')).toBeNull();
    fireEvent.click(library);
    expect(await screen.findByText('License text')).toBeTruthy();
    expect(container.querySelector('.about__doodle')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: /Back|返回/ }));
    fireEvent.click(screen.getByRole('button', { name: /Back|返回/ }));
    expect(container.querySelector('.about__doodle')).not.toBeNull();
    fireEvent.click(screen.getByRole('button', { name: /GPL-3.0/ }));
    expect(container.querySelector('.about__doodle')).toBeNull();
    await waitFor(() => expect(container.querySelector('.license-text')?.textContent).toBe('License text\n\nLicense text'));
    expect(attribution).toHaveBeenCalledWith('LICENSE');
    expect(attribution).toHaveBeenCalledWith('LICENSE-ADDITIONAL-TERMS.md');
  });
  it('lets long license pills grow vertically with padding inside a bounded row', async () => {
    about();
    fireEvent.click(screen.getByRole('button', { name: /Open source attributions|开源软件引用/ }));
    const label = await screen.findByText(expression);
    const pill = label.closest('.about__lic-tag')!;
    const css = getComputedStyle(pill);
    expect(css.height).toBe('auto');
    expect(css.minHeight).toBe('24px');
    expect(css.padding).toBe('6px 8px');
    expect(css.maxWidth).toBe('35%');
    expect(css.flexShrink).toBe('0');
    expect(css.whiteSpace).toBe('normal');
    expect(css.overflowWrap).toBe('anywhere');
  });
});
