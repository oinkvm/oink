import { useSyncExternalStore } from 'react';
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
const listeners = new Set<() => void>();
export const cameraStore = {
    set(v: CameraInfo) { value = v; listeners.forEach((l) => l()); },
    subscribe(l: () => void) { listeners.add(l); return () => { listeners.delete(l); }; },
    get: () => value,
};
export const useCamera = () => useSyncExternalStore(cameraStore.subscribe, cameraStore.get, cameraStore.get);
