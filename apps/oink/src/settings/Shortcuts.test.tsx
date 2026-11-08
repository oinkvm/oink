// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, act } from '@testing-library/react';
import { ShortcutRecorder, spokenKeys } from './Shortcuts';
import { setLanguage } from '../i18n';
import { readFileSync } from 'node:fs';
const appCss = readFileSync('src/app.css', 'utf8');
const backend = vi.hoisted(() => ({call: vi.fn().mockResolvedValue(null), handler: undefined as ((e: any) => void) | undefined}));
vi.mock('../api/client', () => ({call: backend.call, onAgentEvent: async (handler: any) => { backend.handler = handler; return () => { backend.handler = undefined; }; }}));
vi.mock('oink-ui', () => ({animate: vi.fn(), VisuallyHidden: ({children}: any) => <span>{children}</span>}));
afterEach(cleanup);
beforeEach(() => { setLanguage('zh-Hans'); backend.call.mockClear(); });
describe('mouse shortcut recorder', () => {
  it.each(['Mouse3', 'Mouse4', 'Mouse5'].flatMap(key => [false,true].map(ctrl=>({key,ctrl}))))('records $key with Control=$ctrl from native capture', async ({key,ctrl}) => {
    const change = vi.fn();
    render(<ShortcutRecorder platform="mac" value="" onChange={change} onReject={vi.fn()} label="test"/>);
    fireEvent.click(screen.getByRole('button'));
    await act(async () => {});
    const request = backend.call.mock.calls.find(c => c[0] === 'record_mouse_shortcut');
    expect(request).toBeDefined();
    await act(async () => backend.handler?.({type:'mouse_shortcut_recorded',data:{token:request![1].token,keys:ctrl ? ['Ctrl', key] : [key]}}));
    expect(change).toHaveBeenCalledWith(ctrl ? `Control+${key}` : key);
    expect(screen.queryByRole('status')).toBeNull();
  });
  it.each(['Mouse3', 'Mouse4', 'Mouse5'])('shows %s as an accessible SVG keycap', key => {
    render(<ShortcutRecorder platform="win" value={`Control+${key}`} onChange={vi.fn()} onReject={vi.fn()} label="test"/>);
    const label = key === 'Mouse3' ? '鼠标中键' : key === 'Mouse4' ? '鼠标侧键 1' : '鼠标侧键 2';
    const cap = screen.getByLabelText(label);
    expect(cap.tagName.toLowerCase()).toBe('kbd');
    expect(cap.querySelector('svg')).toBeTruthy();
    expect(cap.textContent).toBe('');
  });
  it.each(['mac', 'win'] as const)('centers keyboard keycaps independently of platform font metrics on %s', platform => {
    // The browser integration test renders this same selector in WebView2 and
    // WebKit. Keep the contract visible in the source test as jsdom has no CSS
    // layout engine and reports an empty computed style for imported CSS.
    expect(appCss).toMatch(/\.ok-kbd\s*\{[^}]*display:\s*inline-flex;[^}]*align-items:\s*center;[^}]*justify-content:\s*center;[^}]*line-height:\s*1;/s);
    render(<ShortcutRecorder platform={platform} value="Control+Alt+Shift+1" onChange={vi.fn()} onReject={vi.fn()} label="test"/>);
    const caps = [...screen.getByRole('button').querySelectorAll('kbd')];
    expect(caps).toHaveLength(4);
    expect(caps.every(cap => cap.classList.contains('ok-kbd'))).toBe(true);
  });
  it('keeps the keyboard modifier requirement and rejects left/right mouse recording', () => {
    const change = vi.fn(), reject = vi.fn();
    render(<ShortcutRecorder platform="win" value="" onChange={change} onReject={reject} label="test"/>);
    fireEvent.click(screen.getByRole('button'));
    fireEvent.mouseDown(screen.getByRole('button'), {button:2});
    expect(change).not.toHaveBeenCalled();
    fireEvent.keyDown(screen.getByRole('button'), {key:'A',code:'KeyA'});
    expect(reject).toHaveBeenCalledWith('至少要带一个修饰键');
  });
  it.each(['en', 'ja', 'ko', 'zh-Hans'])('localizes mouse key names in %s', lang => {
    setLanguage(lang);
    expect(spokenKeys('win','Control+Mouse4')).not.toContain('Mouse4');
    expect(spokenKeys('mac','Mouse3')).not.toContain('Mouse3');
    expect(spokenKeys('mac','Mouse5')).not.toContain('Mouse5');
  });
});
