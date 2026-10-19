/** Match the panel's hidden-paint handshake for normal windows. Initial backend
 * state is resolved before React mounts; fonts and scripted corner layout must
 * also be ready before the native shell reveals the first frame. */
export function revealWhenPainted(reveal: () => void): () => void {
  let stopped = false;
  let frame = 0;
  void document.fonts.ready.then(() => {
    if (stopped) return;
    frame = requestAnimationFrame(() => {
      frame = requestAnimationFrame(() => { if (!stopped) reveal(); });
    });
  });
  return () => { stopped = true; cancelAnimationFrame(frame); };
}
