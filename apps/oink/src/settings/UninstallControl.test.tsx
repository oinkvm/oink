// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { UninstallControl } from './UninstallControl';
import { searchSections, sectionsFor } from './schema';
import { setLanguage, tr } from '../i18n';
const uninstall = vi.hoisted(() => vi.fn());
vi.mock('../api/client', () => ({ shell: { uninstall } }));
beforeAll(() => {
    Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: true, addEventListener() {}, removeEventListener() {} }) });
    vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
});
beforeEach(() => { uninstall.mockReset(); setLanguage('en'); });
afterEach(cleanup);

it('is last in General on Mac, absent on Windows and searchable in four languages', () => {
    expect(sectionsFor('mac')[0].items.at(-1)?.id).toBe('uninstall');
    expect(sectionsFor('win').flatMap(s => s.items).some(i => i.id === 'uninstall')).toBe(false);
    for (const [lang, word] of [['zh-Hans', '卸载'], ['en', 'uninstall'], ['ja', 'アンインストール'], ['ko', '제거']]) {
        setLanguage(lang);
        const found = searchSections('mac', word).get('general');
        expect(found === 'all' || found?.has('uninstall')).toBe(true);
        const windows = searchSections('win', word).get('general');
        expect(windows !== 'all' && windows?.has('uninstall')).not.toBe(true);
    }
});
it('requires explicit confirmation and cancellation has no side effects', async () => {
    render(<OinkProvider defaultMotion="reduced"><UninstallControl/></OinkProvider>);
    fireEvent.click(screen.getByRole('button', { name: 'Uninstall Oink' }));
    expect(await screen.findByText('Uninstall Oink?')).toBeTruthy();
    expect(screen.getByText(/This removes the installed Oink virtual microphone/).textContent).toContain('This cannot be undone.');
    expect(uninstall).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(uninstall).not.toHaveBeenCalled();
});
it('keeps removal failures visible, supports retry and sends translated native messages', async () => {
    uninstall.mockRejectedValueOnce('remove_domains').mockResolvedValueOnce(undefined);
    render(<OinkProvider defaultMotion="reduced"><UninstallControl/></OinkProvider>);
    fireEvent.click(screen.getByRole('button', { name: 'Uninstall Oink' }));
    fireEvent.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Uninstall Oink' }));
    expect(await screen.findByRole('alert')).toHaveProperty('textContent', expect.stringContaining('has not been moved to the Trash'));
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Uninstall Oink' }));
    await waitFor(() => expect(uninstall).toHaveBeenCalledTimes(2));
    expect(uninstall.mock.calls[0][0].privacy).toBe('Reset Oink privacy permissions');
});
it('disables confirmation while native preparation is pending', async () => {
    uninstall.mockImplementation(() => new Promise(() => {}));
    render(<OinkProvider defaultMotion="reduced"><UninstallControl/></OinkProvider>);
    fireEvent.click(screen.getByRole('button', { name: 'Uninstall Oink' }));
    const confirm = within(await screen.findByRole('dialog')).getByRole('button', { name: 'Uninstall Oink' });
    fireEvent.click(confirm);
    await waitFor(() => expect((confirm as HTMLButtonElement).disabled).toBe(true));
    expect(screen.getByText(/Preparing to uninstall Oink\./).getAttribute('role')).toBe('status');
    expect((screen.getByRole('button', { name: 'Cancel' }) as HTMLButtonElement).disabled).toBe(true);
});

it.each(['zh-Hans', 'en', 'ja', 'ko'])('explains microphone authorization and allows retry after cancellation in %s', async lang => {
    setLanguage(lang);
    uninstall.mockRejectedValueOnce({ step: 'microphone_plugin', kind: 'cancelled', detail: 'execution error (-128)' }).mockResolvedValueOnce(undefined);
    render(<OinkProvider defaultMotion="reduced"><UninstallControl/></OinkProvider>);
    fireEvent.click(screen.getByRole('button', { name: tr('卸载 Oink') }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: tr('卸载 Oink') }));
    expect(await screen.findByRole('alert')).toHaveProperty('textContent', tr('虚拟麦克风未移除，应用尚未移入废纸篓，可重试'));
    expect(screen.getByRole('alert').textContent).not.toContain('(-128)');
    fireEvent.click(within(dialog).getByRole('button', { name: tr('卸载 Oink') }));
    await waitFor(() => expect(uninstall).toHaveBeenCalledTimes(2));
});
it.each(['authorization_denied', 'verification_failed', 'query_failed', 'other'])('keeps the specific microphone %s error visible', async kind => {
    uninstall.mockRejectedValueOnce({ step: 'microphone_plugin', kind, detail: 'specific system reason' });
    render(<OinkProvider defaultMotion="reduced"><UninstallControl/></OinkProvider>);
    fireEvent.click(screen.getByRole('button', { name: 'Uninstall Oink' }));
    fireEvent.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Uninstall Oink' }));
    expect(await screen.findByRole('alert')).toHaveProperty('textContent',
        'Could not remove the Oink virtual microphone. The app has not been moved to the Trash. You can retry. Reason: specific system reason');
});
