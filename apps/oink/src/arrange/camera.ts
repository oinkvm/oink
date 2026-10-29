import { presets } from 'oink-ui';
import { sameRect } from './geometry';
import type { Camera, Rect } from './geometry';

const KEYS = ['zoom', 'ox', 'oy'] as const;
const zero = (): Camera => ({ zoom: 0, ox: 0, oy: 0 });

/** The inset world view that reproduces the current camera without a jump. */
export function visibleArrangement(camera: Camera, viewport: { w: number; h: number }, padding: number): Rect {
    return {
        x: (padding - camera.ox) / camera.zoom,
        y: (56 - camera.oy) / camera.zoom,
        w: Math.max(1, viewport.w - 2 * padding) / camera.zoom,
        h: Math.max(1, viewport.h - 56 - padding) / camera.zoom,
    };
}

/** Expand gradually: refitting immediately would push an outside pointer ever farther away. */
export function growArrangementView(view: Rect, live: Rect, zoom: number, padding: number, dt: number): Rect {
    // A screen may enter the margin; once it crosses the edge of the canvas, that
    // side targets the whole screen. Targeting only the part past the edge would
    // start from zero speed, and the growth would show only once it was far out.
    const m = padding / zoom;
    const x0 = live.x < view.x - m ? live.x : view.x;
    const y0 = live.y < view.y - 56 / zoom ? live.y : view.y;
    const x1 = live.x + live.w > view.x + view.w + m ? live.x + live.w : view.x + view.w;
    const y1 = live.y + live.h > view.y + view.h + m ? live.y + live.h : view.y + view.h;
    const need = { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
    if (dt <= 0 || sameRect(need, view, .5)) return view;
    const k = 1 - Math.exp(-dt * 6);
    return {
        x: view.x + (need.x - view.x) * k,
        y: view.y + (need.y - view.y) * k,
        w: view.w + (need.w - view.w) * k,
        h: view.h + (need.h - view.h) * k,
    };
}

/** Follows the displayed screen bounds, preserving velocity as that target moves. */
export class SpringCamera {
    private value?: Camera;
    private velocity = zero();

    get() { return this.value; }

    jump(target: Camera) {
        this.value = { ...target };
        this.velocity = zero();
    }

    /** Called after the screen springs, on their animation clock. */
    step(target: Camera, dt: number): boolean {
        if (!this.value || !Number.isFinite(dt)) {
            this.jump(target);
            return false;
        }
        const omega = Math.sqrt(presets.soft.stiffness / presets.soft.mass);
        const decay = Math.exp(-omega * dt);
        for (const key of KEYS) {
            const delta = this.value[key] - target[key];
            const v = this.velocity[key];
            const term = v + omega * delta;
            // Exact critically damped step: the camera adds no oscillation of its own.
            this.value[key] = target[key] + (delta + term * dt) * decay;
            this.velocity[key] = (v - omega * term * dt) * decay;
        }
        const resting = KEYS.every(key => {
            const precision = key === 'zoom' ? 1e-7 : .005;
            return Math.abs(this.value![key] - target[key]) < precision
                && Math.abs(this.velocity[key]) < precision * omega;
        });
        if (resting) this.jump(target);
        return !resting;
    }
}
