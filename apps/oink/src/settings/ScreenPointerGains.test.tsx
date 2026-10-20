// @vitest-environment jsdom
import { cleanup, fireEvent, render, waitFor } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { setLanguage } from '../i18n';
import { ScreenPointerGains } from './ScreenPointerGains';
import { searchSections } from './schema';
const mocks = vi.hoisted(() => ({
  state: {} as Record<string, unknown>, save: vi.fn(), query: vi.fn(), fail: vi.fn(), layout: vi.fn(),
}));
vi.mock('../api/store', () => ({ useAgent: () => mocks.state, agent: { query: mocks.query, fail: mocks.fail } }));
vi.mock('../api/client', () => ({ api: { setScreenRemoteGain: mocks.save, layout: mocks.layout } }));
beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: true, addEventListener() {}, removeEventListener() {} }) });
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
});
beforeEach(() => {
  vi.clearAllMocks();
  mocks.state = { overview: {this_device: {id: 'self'}}, layout: {geometry: {screens: [
    {key: {device: 'self', display: 'one'}, display: {name: 'One'}, remote_gain: 2},
    {key: {device: 'self', display: 'two'}, display: {name: 'Two'}},
    {key: {device: 'peer', display: 'other'}, display: {name: 'Other'}, remote_gain: 4},
  ]}}};
  mocks.save.mockResolvedValue(null);
  mocks.query.mockResolvedValue(null);
});
afterEach(cleanup);
function view() { return <OinkProvider defaultMotion="reduced"><ScreenPointerGains/></OinkProvider>; }
it.each(['en', 'zh-Hans', 'ja', 'ko'] as const)('shows only owned screens and a default gain without writing on load in %s', language => {
  setLanguage(language);
  const result = render(view());
  const sliders = result.getAllByRole('slider');
  expect(sliders).toHaveLength(2);
  expect(sliders[0].getAttribute('aria-valuenow')).toBe('2');
  expect(sliders[1].getAttribute('aria-valuenow')).toBe('1');
  expect(result.queryByText('Other')).toBeNull();
  expect(mocks.save).not.toHaveBeenCalled();
});
it('persists only the selected screen and refreshes the owner snapshot', async () => {
  setLanguage('en');
  const result = render(view());
  fireEvent.keyDown(result.getByRole('slider', {name: 'Remote pointer speed on One'}), {key: 'ArrowRight'});
  await waitFor(() => expect(mocks.save).toHaveBeenCalledExactlyOnceWith({device: 'self', display: 'one'}, 2.1));
  await waitFor(() => expect(mocks.query).toHaveBeenCalledWith('layout', mocks.layout));
});
it('restores the saved value after a rejected edit', async () => {
  mocks.save.mockRejectedValueOnce(new Error('unavailable'));
  const result = render(view());
  fireEvent.keyDown(result.getAllByRole('slider')[0], {key: 'ArrowRight'});
  await waitFor(() => expect(mocks.fail).toHaveBeenCalledWith('unavailable'));
  expect(result.getAllByRole('slider')[0].getAttribute('aria-valuenow')).toBe('2');
});
it('finds the per-screen setting through Settings search', () => {
  setLanguage('en');
  expect(searchSections('mac', 'Remote pointer speed per screen').get('input')).toContain('screenPointerGains');
});
