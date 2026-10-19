import type { Camera, Rect } from './geometry';

/** Round shared edges, not independent widths, so touching screens stay flush. */
export function projectScreenRect(rect: Rect, camera: Camera, pixelRatio: number, origin = { x: 0, y: 0 }): Rect {
    // The canvas can itself sit on a fractional pixel during sidebar motion.
    const snap = (value: number, offset: number) => Math.round((value + offset) * pixelRatio) / pixelRatio - offset;
    const x = snap(camera.ox + rect.x * camera.zoom, origin.x);
    const y = snap(camera.oy + rect.y * camera.zoom, origin.y);
    return {
        x, y,
        w: Math.max(0, snap(camera.ox + (rect.x + rect.w) * camera.zoom, origin.x) - x),
        h: Math.max(0, snap(camera.oy + (rect.y + rect.h) * camera.zoom, origin.y) - y),
    };
}
