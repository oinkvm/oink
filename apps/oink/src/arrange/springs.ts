import { useEffect, useReducer, useRef } from 'react';
import { presets, speed } from 'oink-ui';
import type { SpringPreset } from 'oink-ui';
import type { Rect } from './geometry';
import { sameRect } from './geometry';
export type SpringName = SpringPreset;
const KEYS = ['x', 'y', 'w', 'h'] as const;
interface Item {
    pos: Rect;
    vel: Rect;
    target: Rect;
    preset: SpringName;
    moving: boolean;
}
const zero = (): Rect => ({ x: 0, y: 0, w: 0, h: 0 });
export class SpringRects {
    private items = new Map<string, Item>();
    private raf = 0;
    private last = 0;
    private active = true;
    onFrame: () => void = () => { };
    // Followers run after every screen has advanced, before the frame is rendered.
    // Gesture growth uses wall time; spring followers use the scaled animation time.
    afterStep: (dt: number, elapsed: number) => boolean = () => false;
    lastPreset: SpringName = 'soft';
    has(id: string) { return this.items.has(id); }
    get(id: string): Rect | undefined { return this.items.get(id)?.pos; }
    target(id: string): Rect | undefined { return this.items.get(id)?.target; }
    isMoving(id: string) { return !!this.items.get(id)?.moving; }
    jump(id: string, r: Rect) {
        this.items.set(id, { pos: { ...r }, vel: zero(), target: { ...r }, preset: 'soft', moving: false });
        this.onFrame();
    }
    to(id: string, r: Rect, preset: SpringName = 'soft', from?: Rect) {
        let it = this.items.get(id);
        if (!it) {
            it = { pos: { ...(from ?? r) }, vel: zero(), target: { ...r }, preset, moving: false };
            this.items.set(id, it);
        }
        if (sameRect(it.target, r) && !it.moving && sameRect(it.pos, r))
            return;
        if (!id.startsWith('__') && !id.startsWith('r:'))
            this.lastPreset = preset;
        it.target = { ...r };
        it.preset = preset;
        it.moving = true;
        this.wake();
    }
    remove(id: string) { this.items.delete(id); }
    ids() { return [...this.items.keys()]; }
    setActive(active: boolean) {
        this.active = active;
        if (active) this.wake(); else this.stop();
    }
    wake() {
        if (!this.active || this.raf)
            return;
        this.last = performance.now();
        this.raf = requestAnimationFrame(this.step);
    }
    private step = (now: number) => {
        this.raf = 0;
        const s = speed();
        const dt = Math.min((now - this.last) / 1000, 1 / 30);
        this.last = now;
        let any = false;
        for (const it of this.items.values()) {
            if (!it.moving)
                continue;
            if (s <= 0) {
                it.pos = { ...it.target };
                it.vel = zero();
                it.moving = false;
                continue;
            }
            const { stiffness: k, damping: c, mass: m } = presets[it.preset];
            const total = dt / s;
            const n = Math.max(1, Math.ceil(total * 240));
            const h = total / n;
            for (let i = 0; i < n; i++) {
                for (const key of KEYS) {
                    const a = (-k * (it.pos[key] - it.target[key]) - c * it.vel[key]) / m;
                    it.vel[key] += a * h;
                    it.pos[key] += it.vel[key] * h;
                }
            }
            const scale = Math.max(1, it.target.w / 400);
            const rest = KEYS.every((key) => Math.abs(it.pos[key] - it.target[key]) < 0.05 * scale && Math.abs(it.vel[key]) < 1 * scale);
            if (rest) {
                it.pos = { ...it.target };
                it.vel = zero();
                it.moving = false;
            }
            else
                any = true;
        }
        any = this.afterStep(s > 0 ? dt / s : Infinity, dt) || any;
        this.onFrame();
        if (any) {
            this.last = now;
            this.raf = requestAnimationFrame(this.step);
        }
    };
    stop() { cancelAnimationFrame(this.raf); this.raf = 0; }
}
export function useSpringRects(active = true) {
    const ref = useRef<SpringRects | undefined>(undefined);
    if (!ref.current)
        ref.current = new SpringRects();
    const [, force] = useReducer((n: number) => n + 1, 0);
    useEffect(() => {
        const sys = ref.current!;
        sys.onFrame = force;
        return () => sys.stop();
    }, []);
    useEffect(() => { ref.current!.setActive(active); }, [active]);
    return ref.current;
}
