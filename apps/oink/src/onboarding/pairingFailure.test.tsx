// @vitest-environment jsdom
import { Profiler } from 'react';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { agent } from '../api/store';
import { pairingApi, pairProblem } from '../api/pairing';
import type { PairView, PeerView } from '../api/pairing';
import type { Overview } from '../api/types';
import { usePairing } from '../pairing/usePairing';
import { setLanguage } from '../i18n';
import { Onboarding, type OnboardingProps } from './Onboarding';

const peer = { device: 'remote', name: 'Peer', os: 'macos', remote: { family: 0, address: [192, 0, 2, 1], port: 47470, interface_scope: 0 }, listen: null } satisfies PeerView;
const failed: PairView = { id: 'pair', revision: '2', direction: 'outgoing', phase: 8,
  peer: { address: peer.remote, expected_id: peer.device, verified: null }, attempts_left: 3, feedback: 'waiting', terminal: pairProblem(4) };

beforeEach(() => {
  setLanguage('en');
  vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });
  vi.stubGlobal('getComputedStyle', () => ({ height: '100px', getPropertyValue: () => '1' }));
  Object.defineProperty(window, 'matchMedia', { configurable: true, value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
  // Keep morph animations in flight: reduced motion/jsdom's missing animate API
  // would synchronously remove the stale outgoing list and hide the regression.
  Element.prototype.animate = vi.fn(() => ({ cancel() {} })) as unknown as typeof Element.prototype.animate;
  agent.adopt('overview', { this_device: { id: 'local', fingerprint: 'local' } } as Overview);
  agent.adopt('pairings', { revision: '1', requests: [], sessions: [] });
  agent.adopt('nearby', { revision: '1', rows: [{ ...peer, group: { group: 'remote-group', root: null }, count: 1, addresses: [peer.remote], source: 0, identity: 'advertised', reachable: true, compatible: true, selected: peer.remote, problem: null }] });
  vi.spyOn(pairingApi, 'onboardingConnect').mockResolvedValue(null);
  vi.spyOn(pairingApi, 'pairings').mockImplementation(async () => agent.get().pairings!);
  vi.spyOn(pairingApi, 'ack').mockResolvedValue(null);
});
afterEach(() => { cleanup(); agent.reset(); localStorage.clear(); vi.restoreAllMocks(); vi.unstubAllGlobals(); delete (Element.prototype as { animate?: unknown }).animate; });

it.each((['before', 'after'] as const).flatMap(order => [true, false].map(receipt => ({ order, receipt }))))('never commits the nearby list over an immediate failure $order the start reply (receipt=$receipt)', async ({ order, receipt }) => {
  let started!: (value: { type: 'started'; pair_id: string }) => void;
  vi.spyOn(pairingApi, 'start').mockImplementation(() => new Promise(resolve => { started = resolve; }));
  const commits: { list: boolean; failure: boolean }[] = [];
  function App() {
    const pairing = usePairing();
    const props = { platform: 'win', pairing, perms: {}, onGrant() {}, container: null, onSkip() {}, onFinish() {}, leaving: false, connected: false, version: '0.3.0',
      preferences: { ready: true, report: {}, chooseReport: async () => {}, saveLocal: async () => {} } } as unknown as OnboardingProps;
    return <OinkProvider><Profiler id="onboarding" onRender={() => commits.push({ list: !!document.querySelector('.ob-devs'), failure: !!document.querySelector('.pair__failed') })}><Onboarding {...props}/></Profiler></OinkProvider>;
  }
  render(<App/>);
  fireEvent.click(screen.getByRole('button', { name: 'Next' }));
  await screen.findByText('Help us improve Oink');
  fireEvent.click(screen.getByRole('button', { name: 'Next' }));
  await screen.findByText('Connect your other devices');
  await waitFor(() => expect(document.querySelector('.ob-main')?.hasAttribute('inert')).toBe(false));
  fireEvent.click(screen.getByRole('button', { name: /Connect to Peer/ }));
  const fail = () => {
    agent.receive({ type: 'pairing_changed', data: { revision: '2', view: failed } });
    if (receipt) agent.receive({ type: 'pairing_failed', data: { event_id: '1', pair_id: 'pair', peer: failed.peer, problem: pairProblem(4) } });
  };
  if (order === 'before') act(fail);
  await act(async () => { started({ type: 'started', pair_id: 'pair' }); });
  if (order === 'after') act(fail);
  await screen.findByText('Pairing failed');
  expect(commits.some(c => c.failure)).toBe(true);
  expect(commits.filter(c => c.failure && c.list)).toEqual([]);
  expect(document.querySelector('.ob-devs')).toBeNull();
});
