import type { ArrangementSnapshot } from '../api/layoutProfiles';
import type { Screen } from './geometry';
import { RENDER_SCALE } from './adapter';

/** Convert a uniform computer translation/resize to Placement changes. Native
 * geometry (including the result of a user's screen drag) stays untouched. */
export function snapshotFromScreens(before: ArrangementSnapshot, old: Screen[], next: Screen[]): ArrangementSnapshot {
    const placements = before.placements.map(p => {
        const n = before.native.find(n => n.device === p.device)!;
        const changed = next.some(s => s.device === p.device && old.some(o => o.id === s.id && (s.x !== o.x || s.y !== o.y || s.w !== o.w || s.h !== o.h)));
        if (!changed) return p;
        const primary = n.screens.find(s => s.screen.display === n.primary.display)!;
        const primaryId = `${n.primary.device}:${n.primary.display}`;
        const from = old.find(s => s.id === primaryId), to = next.find(s => s.id === primaryId);
        if (!from || !to) return p;
        const scale = from.w === to.w ? p.scale : p.scale * to.w / from.w;
        // Rebuild the native-zero anchor from the destination. Adding a large
        // displacement to the old origin loses the exact contacted edge again.
        const origin = {
            x: to.x === from.x && scale === p.scale ? p.origin.x : to.x / RENDER_SCALE - primary.origin.x * scale,
            y: to.y === from.y && scale === p.scale ? p.origin.y : to.y / RENDER_SCALE - primary.origin.y * scale,
        };
        return { ...p, origin, scale };
    });
    return { ...before, placements };
}
