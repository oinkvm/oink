// @vitest-environment jsdom
import { useState } from 'react';
import { act, cleanup, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Arrangement } from './Arrangement';
import { growArrangementView, SpringCamera, visibleArrangement } from './camera';
import { cameraStore } from './cameraStore';
import { bounds, fitArrangement } from './geometry';
import type { Screen } from './geometry';

const viewport = { w: 900, h: 600 };
const settings = { showHotZones: false, snapping: false, compact: true, padding: 48, step: 100 };
const initial: Screen[] = [
    { id: 'one', name: 'First', x: 0, y: 0, w: 800, h: 500, wallpaper: 0 },
    { id: 'two', name: 'Second', x: 800, y: 0, w: 800, h: 500, wallpaper: 1 },
];
let now = 0;
let sequence = 0;
const frames = new Map<number, FrameRequestCallback>();
const tick = () => act(() => {
    now += 1000 / 60;
    const pending = [...frames.values()];
    frames.clear();
    pending.forEach(cb => cb(now));
});
const settle = () => {
    for (let n = 0; n < 240 && frames.size; n++) tick();
    expect(frames.size).toBe(0);
};
beforeEach(() => {
    now = 0; sequence = 0; frames.clear();
    vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });
    vi.stubGlobal('PointerEvent', MouseEvent);
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => { frames.set(++sequence, cb); return sequence; });
    vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id));
    vi.spyOn(performance, 'now').mockImplementation(() => now);
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(() => ({
        left: 0, top: 0, x: 0, y: 0, width: viewport.w, height: viewport.h,
        right: viewport.w, bottom: viewport.h, toJSON() {},
    }));
    HTMLElement.prototype.setPointerCapture = () => {};
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

function Canvas({ snapping = false, compact = settings.compact }: { snapping?: boolean; compact?: boolean }) {
    const [screens, setScreens] = useState(initial);
    return <Arrangement screens={screens} onScreensChange={setScreens} selectedId="two" onSelect={() => {}}
        settings={{ ...settings, snapping, compact }} />;
}

function displayed(element: Element) {
    const style = (element as HTMLElement).style;
    const x = parseFloat(style.left), y = parseFloat(style.top);
    return { x, y, w: parseFloat(style.width), h: parseFloat(style.height) };
}

describe('camera follows the active gesture', () => {
    it.each(['move', 'resize'] as const)('gradually zooms out during %s and keeps zooming with a stationary outside pointer', mode => {
        const { container } = render(<Canvas />);
        settle();
        const display = container.querySelectorAll('.scr')[1];
        const r = displayed(display);
        const start = mode === 'move' ? { x: r.x + r.w / 2, y: r.y + r.h / 2 } : { x: r.x + r.w, y: r.y + r.h };
        const handle = mode === 'move' ? display : display.querySelector('[data-c="se"]')!;
        const original = cameraStore.get()!.zoom;
        fireEvent.pointerDown(handle, { button: 0, clientX: start.x, clientY: start.y });
        fireEvent.pointerMove(display, { clientX: start.x + 400, clientY: start.y + (mode === 'resize' ? 250 : 0) });
        expect(cameraStore.get()!.zoom).toBe(original);
        const live = displayed(display);
        const instantFit = fitArrangement(cameraStore.get()!.fit, viewport, settings.padding).zoom;
        expect(live.x + live.w).toBeGreaterThan(viewport.w);
        tick();
        const first = cameraStore.get()!.zoom;
        expect(first).toBeLessThan(original);
        expect(first).toBeGreaterThan(instantFit);
        for (let i = 0; i < 12; i++) tick();
        const held = cameraStore.get()!.zoom;
        expect(held).toBeLessThan(first);
        for (let i = 0; i < 12; i++) tick();
        expect(cameraStore.get()!.zoom).toBeLessThan(held);
        const release = cameraStore.get()!.zoom;
        fireEvent.pointerUp(display);
        expect(cameraStore.get()!.zoom).toBe(release);
        settle();
        expect(cameraStore.get()!.zoom).toBe(fitArrangement(cameraStore.get()!.target, viewport, settings.padding).zoom);
    });

    it('keeps the grabbed point under the pointer every frame as the camera changes', () => {
        const { container } = render(<Canvas />);
        settle();
        const display = container.querySelectorAll('.scr')[1];
        const r = displayed(display);
        const before = cameraStore.get()!;
        const original = before.zoom;
        const fitted = fitArrangement(before.fit, viewport, settings.padding);
        const grab = { x: 210, y: 170 };
        const virtualGrab = {
            x: grab.x + (r.x - fitted.ox) / original - initial[1].x,
            y: grab.y + (r.y - fitted.oy) / original - initial[1].y,
        };
        fireEvent.pointerDown(display, { button: 0, clientX: r.x + grab.x * original, clientY: r.y + grab.y * original });
        const pointer = { x: 1040, y: 620 };
        fireEvent.pointerMove(display, { clientX: pointer.x, clientY: pointer.y });
        for (let i = 0; i < 30; i++) {
            tick();
            const zoom = cameraStore.get()!.zoom;
            const live = displayed(display);
            // World movement rounds to a unit; painting adds half a physical pixel.
            expect(Math.abs(live.x + virtualGrab.x * zoom - pointer.x)).toBeLessThanOrEqual(zoom / 2 + .5 + 1e-8);
            expect(Math.abs(live.y + virtualGrab.y * zoom - pointer.y)).toBeLessThanOrEqual(zoom / 2 + .5 + 1e-8);
        }
        expect(cameraStore.get()!.zoom).toBeLessThan(original * .7);
    });

    it('allows the screen into the margin, then stops growing when it is brought back inside', () => {
        const { container } = render(<Canvas />);
        settle();
        const display = container.querySelectorAll('.scr')[1];
        fireEvent.pointerDown(display, { button: 0, clientX: 600, clientY: 300 });
        fireEvent.pointerMove(display, { clientX: 630, clientY: 300 });
        const original = cameraStore.get()!.zoom;
        for (let i = 0; i < 10; i++) tick();
        expect(cameraStore.get()!.zoom).toBe(original);
        fireEvent.pointerMove(display, { clientX: 1040, clientY: 300 });
        for (let i = 0; i < 10; i++) tick();
        const outside = cameraStore.get()!.zoom;
        expect(outside).toBeLessThan(original);
        fireEvent.pointerMove(display, { clientX: 450, clientY: 300 });
        for (let i = 0; i < 10; i++) tick();
        expect(cameraStore.get()!.zoom).toBe(outside);
    });

    it.each([0, 4])('uses gesture elapsed time when animation speed is %s', speed => {
        const { container } = render(<Canvas />);
        settle();
        document.documentElement.style.setProperty('--ok-ms', String(speed));
        try {
            const display = container.querySelectorAll('.scr')[1];
            const cam = fitArrangement(bounds(initial), viewport, settings.padding);
            const view = visibleArrangement(cam, viewport, settings.padding);
            fireEvent.pointerDown(display, { button: 0, clientX: 600, clientY: 300 });
            fireEvent.pointerMove(display, { clientX: 1040, clientY: 300 });
            const live = { ...initial[1], x: 800 + 440 / cam.zoom };
            const next = growArrangementView(view, live, cam.zoom, settings.padding, 1 / 60);
            tick();
            expect(cameraStore.get()!.zoom).toBeCloseTo(fitArrangement(next, viewport, settings.padding).zoom, 12);
        } finally {
            document.documentElement.style.removeProperty('--ok-ms');
        }
    });
});

describe('camera follows animated screens', () => {
    it.each(['move', 'resize'] as const)('starts a %s release without a zoom jump and follows each animated bounding box', mode => {
        const step = vi.spyOn(SpringCamera.prototype, 'step');
        const { container } = render(<Canvas />);
        settle();
        const display = container.querySelectorAll('.scr')[1];
        const handle = mode === 'move' ? display : display.querySelector('[data-c="nw"]')!;
        const start = cameraStore.get()!;
        fireEvent.pointerDown(handle, { button: 0, clientX: 600, clientY: 300 });
        fireEvent.pointerMove(display, { clientX: mode === 'move' ? 1200 : 200, clientY: 300 });
        tick();
        expect(cameraStore.get()!.zoom).toBeLessThanOrEqual(start.zoom);
        const release = cameraStore.get()!;
        fireEvent.pointerUp(display, { clientX: mode === 'move' ? 1200 : 200, clientY: 300 });
        expect(cameraStore.get()!.zoom).toBe(release.zoom);
        expect(cameraStore.get()!.fit).toEqual(release.fit);
        const final = cameraStore.get()!.target;
        expect(release.fit).not.toEqual(final);
        for (let i = 0; i < 12; i++) {
            const before = cameraStore.get()!;
            tick();
            const after = cameraStore.get()!;
            const target = step.mock.lastCall![0];
            expect(target).toEqual(fitArrangement(after.fit, viewport, settings.padding));
            if (i === 0) {
                expect(after.fit).not.toEqual(before.fit);
                expect(Math.abs(after.zoom - before.zoom)).toBeLessThanOrEqual(Math.abs(target.zoom - before.zoom) / 10 + 1e-9);
                expect(target).not.toEqual(fitArrangement(final, viewport, settings.padding));
            }
        }
        settle();
        expect(cameraStore.get()!.fit).toEqual(final);
        expect(cameraStore.get()!.zoom).toBe(fitArrangement(final, viewport, settings.padding).zoom);
    });

    it('follows keyboard motion and resize from their displayed positions', () => {
        const step = vi.spyOn(SpringCamera.prototype, 'step');
        // With auto-compaction a key move away from the neighbour settles back.
        render(<Canvas compact={false} />);
        settle();
        for (const ctrlKey of [false, true]) {
            const before = cameraStore.get()!;
            fireEvent.keyDown(document.body, { key: 'ArrowRight', ctrlKey });
            expect(cameraStore.get()!.zoom).toBe(before.zoom);
            tick();
            const after = cameraStore.get()!;
            expect(after.fit).not.toEqual(before.fit);
            expect(step.mock.lastCall![0]).toEqual(fitArrangement(after.fit, viewport, settings.padding));
            expect(after.fit).not.toEqual(after.target);
            settle();
            expect(cameraStore.get()!.zoom).toBe(fitArrangement(cameraStore.get()!.target, viewport, settings.padding).zoom);
        }
    });

    it('continues fitting after a drag with no screen spring and honours reduced motion', () => {
        const { container } = render(<Canvas />);
        settle();
        const display = container.querySelectorAll('.scr')[1];
        fireEvent.pointerDown(display, { button: 0, clientX: 600, clientY: 300 });
        // Moving down partially intersects the other screen: compact keeps this projection.
        fireEvent.pointerMove(display, { clientX: 600, clientY: 400 });
        const before = cameraStore.get()!;
        fireEvent.pointerUp(display, { clientX: 600, clientY: 400 });
        expect(cameraStore.get()!.fit).toEqual(cameraStore.get()!.target);
        expect(cameraStore.get()!.zoom).toBe(before.zoom);
        settle();
        const settled = cameraStore.get()!;
        expect(settled.zoom).toBe(fitArrangement(settled.fit, viewport, settings.padding).zoom);
        document.documentElement.style.setProperty('--ok-ms', '0');
        fireEvent.keyDown(document.body, { key: 'ArrowRight' });
        tick();
        expect(cameraStore.get()!.zoom).toBe(fitArrangement(cameraStore.get()!.target, viewport, settings.padding).zoom);
        document.documentElement.style.removeProperty('--ok-ms');
    });
});

describe('resize snapping setting', () => {
    it.each([false, true])('only snaps moving edges when snapping=%s', snapping => {
        const { container } = render(<Canvas snapping={snapping} />);
        settle();
        const zoom = cameraStore.get()!.zoom;
        const display = container.querySelectorAll('.scr')[1];
        fireEvent.pointerDown(display.querySelector('[data-c="se"]')!, { button: 0, clientX: 700, clientY: 400 });
        // Project to a proportional 795 x 497 resize; neighbour bottom is 500.
        fireEvent.pointerMove(display, { clientX: 700 - 5 * zoom, clientY: 400 - 3.125 * zoom });
        const live = cameraStore.get()!.fit;
        expect(live.w).toBe(snapping ? bounds(initial).w : 1595);
        expect(cameraStore.get()!.zoom).toBe(zoom);
        expect(container.querySelector('[data-active="resize"]')).not.toBeNull();
    });
});

it('settles a critically damped camera exactly without oscillating past a fixed target', () => {
    const camera = new SpringCamera();
    camera.jump({ zoom: 1, ox: 100, oy: 100 });
    const target = { zoom: .5, ox: 0, oy: 0 };
    let moving = true;
    for (let i = 0; i < 240 && moving; i++) {
        moving = camera.step(target, 1 / 60);
        expect(camera.get()!.zoom).toBeGreaterThanOrEqual(target.zoom);
        expect(camera.get()!.ox).toBeGreaterThanOrEqual(0);
    }
    expect(moving).toBe(false);
    expect(camera.get()).toEqual(target);
});

it.each([48, 24])('reconstructs the camera view and grows it at the reference rate with %spx margins', padding => {
    const cam = fitArrangement(bounds(initial), viewport, padding);
    const view = visibleArrangement(cam, viewport, padding);
    expect(fitArrangement(view, viewport, padding)).toEqual(cam);
    const live = { x: view.x + view.w, y: view.y, w: 800, h: 500 };
    expect(growArrangementView(view, live, cam.zoom, padding, 0)).toBe(view);
    const next = growArrangementView(view, live, cam.zoom, padding, 1 / 60);
    expect(next.w).toBeCloseTo(view.w + live.w * (1 - Math.exp(-6 / 60)), 10);
    expect(next.x).toBe(view.x);
    expect(next.y).toBe(view.y);
});

it.each([48, 24])('starts growing as soon as a screen crosses the visible canvas edge with %spx margins', padding => {
    const cam = fitArrangement(bounds(initial), viewport, padding);
    const view = visibleArrangement(cam, viewport, padding);
    const k = 1 - Math.exp(-6 / 60), px = 2 / cam.zoom;
    // Touching the card's right edge and its top edge (the 56px band) is still inside.
    const right = { x: view.x + view.w + padding / cam.zoom - 800, y: view.y, w: 800, h: 500 };
    const top = { x: view.x, y: view.y - 56 / cam.zoom, w: 800, h: 500 };
    expect(growArrangementView(view, right, cam.zoom, padding, 1 / 60)).toBe(view);
    // Two pixels past an edge, the whole screen becomes the target at once.
    const outRight = growArrangementView(view, { ...right, x: right.x + px }, cam.zoom, padding, 1 / 60);
    expect(outRight.w).toBeCloseTo(view.w + (padding / cam.zoom + px) * k, 10);
    expect(outRight.x).toBe(view.x);
    expect(outRight.h).toBe(view.h);
    expect(growArrangementView(view, top, cam.zoom, padding, 1 / 60)).toBe(view);
    const outTop = growArrangementView(view, { ...top, y: top.y - px }, cam.zoom, padding, 1 / 60);
    expect(outTop.y).toBeCloseTo(view.y - (56 / cam.zoom + px) * k, 10);
    expect(outTop.h).toBeCloseTo(view.h + (56 / cam.zoom + px) * k, 10);
    expect(outTop.w).toBe(view.w);
});
