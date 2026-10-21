// Shared by the diagnostics charts so a device keeps its colour across views.
const colors = ['var(--ok-info)', 'var(--ok-success)', 'var(--ok-warning)', '#C77DFF', 'var(--ok-danger)', '#14B8A6', '#FF7A2F'];
const assigned = new Map<string, string>();
export function deviceColor(id: string) {
    if (!assigned.has(id)) assigned.set(id, colors[assigned.size % colors.length]);
    return assigned.get(id)!;
}
