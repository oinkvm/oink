/** Native activation can give WKWebView's first/restored button :focus-visible
 * before any keyboard input (observed on macOS 26). Keep its logical focus,
 * but request no ring until the user interacts. WebKit ignores focus options
 * on an already-focused element, so release it before applying focusVisible.
 * Older engines that ignore focusVisible release the incidental focus instead.
 * Tab and dialog focus after user input retain the browser's normal heuristics. */
export function quietActivationFocus(): () => void {
  let activating = true;
  let frame = 0;
  let refocusing = false;
  const quiet = () => {
    const target = document.activeElement;
    if (!activating || !(target instanceof HTMLElement) || target === document.body) return;
    if (!target.matches(':focus-visible')) return;
    let supported = false;
    refocusing = true;
    target.blur();
    target.focus({
      preventScroll: true,
      get focusVisible() { supported = true; return false; },
    } as FocusOptions);
    refocusing = false;
    if (!supported) target.blur();
  };
  // Finish native focus dispatch before adjusting the control, then clear its
  // ring before paint.
  // This never gates native reveal on a hidden WebView's animation frame.
  const requestQuiet = () => {
    if (!activating || refocusing) return;
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(() => { frame = 0; quiet(); });
  };
  const activate = () => { activating = true; requestQuiet(); };
  const interact = () => { activating = false; cancelAnimationFrame(frame); frame = 0; };
  // This is the same logical focus. Do not commit editable fields or close
  // focus scopes through their blur handlers during the WebKit refocus pair.
  const preserveFocus = (event: Event) => { if (refocusing) event.stopImmediatePropagation(); };
  const focusEvents = ['blur', 'focus', 'focusin', 'focusout'];
  focusEvents.forEach(type => document.addEventListener(type, preserveFocus, true));
  window.addEventListener('focus', activate);
  document.addEventListener('focusin', requestQuiet);
  document.addEventListener('keydown', interact, true);
  document.addEventListener('pointerdown', interact, true);
  quiet();
  return () => {
    interact();
    focusEvents.forEach(type => document.removeEventListener(type, preserveFocus, true));
    window.removeEventListener('focus', activate);
    document.removeEventListener('focusin', requestQuiet);
    document.removeEventListener('keydown', interact, true);
    document.removeEventListener('pointerdown', interact, true);
  };
}
