// @vitest-environment jsdom
import { beforeEach, expect, it, vi } from 'vitest';

const { start, render, createRoot, setLanguage, get } = vi.hoisted(() => {
  const render = vi.fn();
  return { start: vi.fn(), render, createRoot: vi.fn(() => ({ render })), setLanguage: vi.fn(), get: vi.fn() };
});
vi.mock('react-dom/client', () => ({ createRoot }));
vi.mock('./App', () => ({ App: () => null }));
vi.mock('./api/store', () => ({ start, agent: { get } }));
vi.mock('./i18n', () => ({ setLanguage }));
beforeEach(() => {
  vi.resetModules(); vi.clearAllMocks();
  document.body.innerHTML = '<div id="root"></div>';
  history.replaceState(null, '', '/?view=main');
  get.mockReturnValue({ localPreferences: { ui_language: 'ja' } });
});
it.each([false, true])('resolves initial backend state before mounting (onboarding_done=%s)', async done => {
  let resolve!: () => void;
  start.mockReturnValue(new Promise<void>(r => { resolve = r; }));
  await import('./main');
  expect(start).toHaveBeenCalledWith('main');
  expect(createRoot).not.toHaveBeenCalled();
  get.mockReturnValue({ overview: { onboarding_done: done }, localPreferences: { ui_language: 'ja' } });
  resolve(); await vi.waitFor(() => expect(render).toHaveBeenCalledOnce());
  expect(setLanguage).toHaveBeenCalledWith('ja');
  expect(setLanguage.mock.invocationCallOrder[0]).toBeLessThan(render.mock.invocationCallOrder[0]);
});
