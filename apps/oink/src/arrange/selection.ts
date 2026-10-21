/** How a screen was picked: a plain click, Cmd/Ctrl+click, or a double-click. */
export type SelectMode = 'plain' | 'toggle' | 'device';

/**
 * The canvas selection model (user decisions 2026-10-21). A selection is a set of
 * screen IDs made only of whole computers, with one exception: a single screen of a
 * multi-screen computer that writes its system arrangement (`single`), which moves
 * on its own. A plain click selects that single screen or else the whole computer;
 * a double-click selects the whole computer; Cmd/Ctrl+click adds or removes the
 * clicked screen's whole computer, so a partial computer never joins a group.
 */
export function nextSelection(current: readonly string[], screens: readonly { id: string; device?: string }[], id: string, mode: SelectMode, single: (id: string) => boolean): string[] {
  const deviceOf = (screen: string) => screens.find(s => s.id === screen)?.device ?? screen;
  const whole = (device: string) => screens.filter(s => (s.device ?? s.id) === device).map(s => s.id);
  if (mode === 'device') return whole(deviceOf(id));
  if (mode === 'plain') return single(id) ? [id] : whole(deviceOf(id));
  const devices = new Set(current.filter(c => screens.some(s => s.id === c)).map(deviceOf));
  if (current.includes(id)) devices.delete(deviceOf(id)); else devices.add(deviceOf(id));
  return screens.filter(s => devices.has(s.device ?? s.id)).map(s => s.id);
}
