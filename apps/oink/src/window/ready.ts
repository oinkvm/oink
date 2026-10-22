/** React has committed the initial state/theme before this effect runs. Fonts
 * and synchronous layout can finish while the native window is hidden. Never
 * wait for rAF here: WKWebView stops frames in invisible/occluded windows, and
 * WebView2 may do the same. Visibility must enable paint, not depend on it.
 * Native reveal preserves the transparent gutter and the themed DOM surface. */
export function revealWhenReady(reveal: () => void): () => void {
  let stopped = false;
  // Start font/layout work without requiring a rendering opportunity.
  document.documentElement.getBoundingClientRect();
  void document.fonts.ready.then(() => {
    if (stopped) return;
    document.documentElement.getBoundingClientRect();
    reveal();
  });
  return () => { stopped = true; };
}
