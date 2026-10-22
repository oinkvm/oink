// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { quietActivationFocus } from './activationFocus';

let stop: (() => void) | undefined;
let frames: Map<number, FrameRequestCallback>;
beforeEach(() => {
  frames = new Map(); let sequence = 0;
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => { frames.set(++sequence, callback); return sequence; });
  vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id));
});
afterEach(() => { stop?.(); document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
function paint() { const pending = [...frames.values()]; frames.clear(); pending.forEach(callback => callback(0)); }

function controls(supported = true) {
  document.body.innerHTML = '<button>Sidebar</button><button>Settings</button><input>';
  const original = HTMLElement.prototype.focus;
  const focus = vi.spyOn(HTMLElement.prototype, 'focus').mockImplementation(function(this: HTMLElement, options?: FocusOptions) {
    if (supported) void (options as FocusOptions & { focusVisible?: boolean })?.focusVisible;
    original.call(this, options);
  });
  return { first: document.querySelector('button')!, next: document.querySelectorAll('button')[1], input: document.querySelector('input')!, focus };
}

it('quiets initial and restored native focus without changing the focused control', () => {
  const { first, next, focus } = controls();
  first.focus(); stop = quietActivationFocus();
  expect(focus).toHaveBeenLastCalledWith({ preventScroll: true, focusVisible: false });
  expect(document.activeElement).toBe(first);
  // WebKit can choose a control after the native window focus event.
  window.dispatchEvent(new Event('focus'));
  next.focus();
  paint();
  expect(focus).toHaveBeenLastCalledWith({ preventScroll: true, focusVisible: false });
  expect(document.activeElement).toBe(next);
});

it.each(['keydown', 'pointerdown'])('leaves focus after real %s interaction to browser heuristics', type => {
  const { first, next, focus } = controls(); stop = quietActivationFocus();
  first.focus(); focus.mockClear();
  document.dispatchEvent(type === 'keydown' ? new KeyboardEvent(type, { key: 'Tab', bubbles: true }) : new Event(type, { bubbles: true }));
  next.focus();
  paint();
  expect(focus).toHaveBeenCalledExactlyOnceWith();
  window.dispatchEvent(new Event('focus'));
  paint();
  expect(focus).toHaveBeenLastCalledWith({ preventScroll: true, focusVisible: false });
});

it('preserves editing focus and selection on activation in supporting WebKit', () => {
  const { input } = controls();
  input.value = 'device'; input.focus(); input.setSelectionRange(2, 4);
  const blur = vi.fn(), focus = vi.fn();
  input.addEventListener('blur', blur); input.addEventListener('focus', focus);
  stop = quietActivationFocus();
  window.dispatchEvent(new Event('focus'));
  paint();
  expect(document.activeElement).toBe(input);
  expect([input.selectionStart, input.selectionEnd]).toEqual([2, 4]);
  expect(blur).not.toHaveBeenCalled(); expect(focus).not.toHaveBeenCalled();
});

it('releases incidental control focus in engines that ignore focusVisible', () => {
  const { first, next } = controls(false); stop = quietActivationFocus();
  const blur = vi.fn(); first.addEventListener('blur', blur);
  first.focus(); paint(); expect(document.activeElement).toBe(document.body);
  expect(blur).toHaveBeenCalledOnce();
  document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true }));
  next.focus(); expect(document.activeElement).toBe(next);
});

it('removes listeners when the window unmounts', () => {
  const { first, focus } = controls(); stop = quietActivationFocus(); stop();
  first.focus();
  expect(focus).toHaveBeenCalledExactlyOnceWith();
});

it('cancels pending activation focus when the window unmounts', () => {
  const { first, focus } = controls(); stop = quietActivationFocus();
  first.focus(); stop(); focus.mockClear(); paint();
  expect(focus).not.toHaveBeenCalled();
  expect(document.activeElement).toBe(first);
});
