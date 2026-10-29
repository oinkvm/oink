// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { agent } from '../api/store';
import type { PairView } from '../api/pairing';
import { setLanguage } from '../i18n';
import { usePairing, type Pairing } from './usePairing';
import { ManualSearch, PairDialog } from './PairDialog';
beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
});
afterEach(() => { cleanup(); agent.reset(); setLanguage('en'); });
function Dialog() { const pairing = usePairing(); return <PairDialog pairing={pairing} container={null} />; }
describe('pairing trust note', () => {
  it.each(['en', 'zh-Hans'] as const)('keeps the tooltip inline with correct word spacing in %s', async language => {
    setLanguage(language);
    const address = { family: 0, address: [192, 0, 2, 1], port: 47470, interface_scope: 0 };
    const view: PairView = { id: 'pair', revision: '1', direction: 'incoming', phase: 2, attempts_left: 3, feedback: 'waiting', terminal: null, peer: { address, expected_id: 'peer', verified: { device: 'peer', name: 'Peer', os: 'macos', remote: address, listen: null } } };
    agent.adopt('pairings', { revision: '1', requests: [], sessions: [view] });
    render(<OinkProvider defaultMotion="reduced"><Dialog /></OinkProvider>);
    await waitFor(() => expect(document.querySelector('.pair__note')?.textContent).toBe(language === 'en'
      ? 'After pairing, the peer can control this device remotely. Make sure you trust this device'
      : '完成配对后，对方可以远程操控本机，请确保信任此设备'));
    const trigger = document.querySelector('.pair__peer')!;
    expect(trigger.textContent).toBe(language === 'en' ? 'this device' : '此设备');
    expect(trigger.getAttribute('tabindex')).toBe('0');
    expect(trigger.getAttribute('aria-label')).toContain('192.0.2.1:47470');
  });
});


it('opens the manual-IP result context menu and blocks that peer', async () => {
  const peer = { id: 'peer', device: 'peer', name: 'Peer', compatible: true, os: 'mac', ip: '192.0.2.1', port: 47470, deviceId: 'AAAA-BBBB-CCCC' };
  const block = vi.fn().mockResolvedValue(undefined);
  const pairing = { defaultPort: 47470, actions: { search: vi.fn().mockResolvedValue(peer), pairFound: vi.fn(), block } } as unknown as Pairing;
  render(<OinkProvider defaultMotion="reduced"><ManualSearch pairing={pairing}/></OinkProvider>);
  fireEvent.change(screen.getByRole('textbox', { name: 'IP address' }), { target: { value: '192.0.2.1' } });
  fireEvent.click(screen.getByRole('button', { name: 'Search' }));
  const result = await screen.findByText('Peer');
  fireEvent.contextMenu(result);
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Block' }));
  expect(block).toHaveBeenCalledExactlyOnceWith(peer);
});
