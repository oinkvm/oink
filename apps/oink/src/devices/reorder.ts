/** Where a moved online device lands in the group-wide order (backend `DeviceOrderPlacement`). */
export type Placement = { above: string } | { after: string };

/** Dropping `id` into `slot` of the online list without it: above that row, or after the last row. */
export function placementFor(ids: readonly string[], id: string, slot: number): Placement | null {
    const others = ids.filter(other => other !== id);
    if (!others.length || !ids.includes(id))
        return null;
    const next = [...others.slice(0, slot), id, ...others.slice(slot)];
    if (next.every((other, index) => other === ids[index]))
        return null;
    return slot < others.length ? { above: others[slot] } : { after: others[others.length - 1] };
}

/** The online list as it looks once `placement` applies; shown until the next device cards arrive. */
export function applyPlacement(ids: readonly string[], id: string, placement: Placement): string[] {
    const others = ids.filter(other => other !== id);
    const anchor = 'above' in placement ? others.indexOf(placement.above) : others.indexOf(placement.after) + 1;
    return [...others.slice(0, anchor), id, ...others.slice(anchor)];
}

/** Alt+↑/↓ moves the focused online row by one place. */
export function stepPlacement(ids: readonly string[], id: string, step: -1 | 1): Placement | null {
    const index = ids.indexOf(id);
    return index < 0 ? null : placementFor(ids, id, Math.max(0, index + step));
}

/** Insertion slot for a pointer at `y`, from the vertical midpoints of the other rows. */
export function slotAt(midpoints: readonly number[], y: number): number {
    return midpoints.filter(mid => mid < y).length;
}
