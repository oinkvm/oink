import type { Rect } from './geometry';
export interface CameraInfo {
    zoom: number;
    fit: Rect;
    target: Rect;
    viewport: {
        w: number;
        h: number;
    };
}
let value: CameraInfo | null = null;
/** Test observation of the canvas camera. */
export const cameraStore = {
    set(v: CameraInfo) { value = v; },
    get: () => value,
};
