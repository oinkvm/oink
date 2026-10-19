// @vitest-environment jsdom
import { useState } from 'react';
import { act, cleanup, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Arrangement } from './Arrangement';
import { cameraStore } from './cameraStore';
import type { Screen } from './geometry';

const native = vi.hoisted(() => ({ invoke: vi.fn().mockResolvedValue(undefined), enabled: true }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke, isTauri: () => native.enabled }));
let now = 0;
let sequence = 0;
const frames = new Map<number, FrameRequestCallback>();
const tick = () => act(() => {
    now += 1000 / 60;
    const pending = [...frames.values()];
    frames.clear();
    pending.forEach(cb => cb(now));
});
const initial: Screen[] = [
    { id: 'one', name: 'First', x: 0, y: 0, w: 800, h: 500, wallpaper: 0 },
    { id: 'two', name: 'Second', x: 1000, y: 0, w: 800, h: 625, wallpaper: 1 },
];
function Canvas({ snapping = true, screens: seed = initial }: { snapping?: boolean; screens?: Screen[] }) {
    const [screens, setScreens] = useState(seed);
    return <Arrangement screens={screens} onScreensChange={setScreens} selectedId="one" onSelect={() => {}}
        settings={{ snapping, showHotZones: false, compact: false, padding: 48, step: 10 }} />;
}
beforeEach(() => {
    now = 0; sequence = 0; frames.clear();
    native.enabled = true;
    native.invoke.mockClear();
    vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });
    vi.stubGlobal('PointerEvent', MouseEvent);
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => { frames.set(++sequence, cb); return sequence; });
    vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id));
    vi.spyOn(performance, 'now').mockImplementation(() => now);
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(() => ({
        left: 0, top: 0, x: 0, y: 0, width: 900, height: 600, right: 900, bottom: 600, toJSON() {},
    }));
    HTMLElement.prototype.setPointerCapture = () => {};
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
function drag(snapping = true, mode: 'move' | 'resize' = 'move') {
    const { container } = render(<Canvas snapping={snapping} />);
    const display = container.querySelector('.scr')!;
    const zoom = cameraStore.get()!.zoom;
    const handle = mode === 'move' ? display : display.querySelector('[data-c="se"]')!;
    fireEvent.pointerDown(handle, { button: 0, clientX: 300, clientY: 300 });
    return {
        move(x: number, y = mode === 'resize' ? x * .625 : 0) {
            fireEvent.pointerMove(display, { clientX: 300 + x * zoom, clientY: 300 + y * zoom });
        },
        end() { fireEvent.pointerUp(display); },
    };
}

describe('alignment haptic command', () => {
    it('keeps a held alignment through camera growth and uses the live zoom for the next engagement', () => {
        const { container } = render(<Canvas />);
        const [display, neighbour] = container.querySelectorAll<HTMLElement>('.scr');
        const original = cameraStore.get()!.zoom;
        // The vertical grab point is at the view centre, so horizontal camera
        // growth preserves the neighbour's top alignment under this pointer.
        const start = { x: 48 + 400 * original, y: 304 };
        fireEvent.pointerDown(display, { button: 0, clientX: start.x, clientY: start.y });
        fireEvent.pointerMove(display, { clientX: start.x, clientY: start.y + 80 * original });
        fireEvent.pointerMove(display, { clientX: 930, clientY: start.y });
        expect(native.invoke).toHaveBeenCalledTimes(1);
        for (let i = 0; i < 3; i++) tick();
        const zoom = cameraStore.get()!.zoom;
        expect(zoom).toBeLessThan(original);
        expect(native.invoke).toHaveBeenCalledTimes(1);
        const [nx, ny] = neighbour.style.transform.match(/-?[\d.]+/g)!.map(Number);
        now = 200;
        fireEvent.pointerMove(display, { clientX: nx - 400 * zoom + 15, clientY: ny + (312.5 + 80) * zoom });
        expect(native.invoke).toHaveBeenCalledTimes(1);
        // Eight canvas pixels is within the snap threshold at the current zoom.
        fireEvent.pointerMove(display, { clientX: nx - 400 * zoom + 8, clientY: ny + (312.5 + 80) * zoom });
        expect(native.invoke).toHaveBeenCalledTimes(2);
        const x = Number(display.style.transform.match(/-?[\d.]+/g)![0]);
        expect(x + parseFloat(display.style.width)).toBeCloseTo(nx, 8);
        tick();
        fireEvent.pointerUp(display);
        expect(native.invoke).toHaveBeenCalledTimes(2);
    });

    it.each(['move', 'resize'] as const)('fires once when %s snapping engages, never while held or released', mode => {
        const pointer = drag(true, mode);
        expect(native.invoke).not.toHaveBeenCalled();
        pointer.move(195);
        expect(native.invoke).toHaveBeenCalledExactlyOnceWith('shell_alignment_haptic');
        now = 200;
        pointer.move(194);
        pointer.move(200); // Exact alignment must still report the same engagement.
        expect(native.invoke).toHaveBeenCalledTimes(1);
        pointer.move(100);
        expect(native.invoke).toHaveBeenCalledTimes(1);
        pointer.move(195);
        expect(native.invoke).toHaveBeenCalledTimes(2);
        pointer.end();
        expect(native.invoke).toHaveBeenCalledTimes(2);
    });

    it('fires for a newly aligned axis while the other axis stays snapped', () => {
        const pointer = drag();
        pointer.move(195, 80);
        expect(native.invoke).toHaveBeenCalledTimes(1);
        now = 200;
        pointer.move(195, 5);
        expect(native.invoke).toHaveBeenCalledTimes(2);
        now = 400;
        pointer.move(194, 0);
        expect(native.invoke).toHaveBeenCalledTimes(2);
    });

    it('throttles rapid disengage and reengage without delayed buzzing', () => {
        const pointer = drag();
        pointer.move(195);
        now = 50;
        pointer.move(100);
        pointer.move(195);
        expect(native.invoke).toHaveBeenCalledTimes(1);
        now = 200;
        pointer.move(194);
        expect(native.invoke).toHaveBeenCalledTimes(1);
        pointer.move(100);
        pointer.move(195);
        expect(native.invoke).toHaveBeenCalledTimes(2);
    });

    it.each(['move', 'resize'] as const)('does not invoke for %s when snapping is off', mode => {
        const pointer = drag(false, mode);
        pointer.move(195);
        pointer.move(200);
        pointer.move(100);
        now = 200;
        pointer.move(195);
        pointer.end();
        expect(native.invoke).not.toHaveBeenCalled();
    });

    it.each([true, false])('keyboard neighbour-edge contact respects snapping=%s', snapping => {
        render(<Canvas snapping={snapping} />);
        fireEvent.keyDown(document.body, { key: 'ArrowRight' }); // 100: still separated.
        expect(native.invoke).not.toHaveBeenCalled();
        fireEvent.keyDown(document.body, { key: 'ArrowRight' }); // 200: exact neighbour edge.
        expect(native.invoke).toHaveBeenCalledTimes(snapping ? 1 : 0);
        now = 200;
        fireEvent.keyDown(document.body, { key: 'ArrowRight' }); // Blocked, no new engagement.
        expect(native.invoke).toHaveBeenCalledTimes(snapping ? 1 : 0);
    });

    it('fires when a keyboard step is clamped to a neighbour edge', () => {
        render(<Canvas screens={[initial[0], { ...initial[1], x: 1050 }]} />);
        fireEvent.keyDown(document.body, { key: 'ArrowRight' });
        fireEvent.keyDown(document.body, { key: 'ArrowRight' });
        expect(native.invoke).not.toHaveBeenCalled();
        fireEvent.keyDown(document.body, { key: 'ArrowRight' });
        expect(native.invoke).toHaveBeenCalledExactlyOnceWith('shell_alignment_haptic');
    });

    it('does not buzz when resizing starts on an already aligned edge', () => {
        const { container } = render(<Canvas screens={[initial[0], { ...initial[1], x: 800 }]} />);
        const display = container.querySelector('.scr')!;
        const zoom = cameraStore.get()!.zoom;
        fireEvent.pointerDown(display.querySelector('[data-c="se"]')!, { button: 0, clientX: 300, clientY: 300 });
        fireEvent.pointerMove(display, { clientX: 300 + 5 * zoom, clientY: 300 + 3.125 * zoom });
        expect(native.invoke).not.toHaveBeenCalled();
    });

    it('is a no-op in browser development', () => {
        native.enabled = false;
        drag().move(195);
        expect(native.invoke).not.toHaveBeenCalled();
    });
});

describe('computer-group alignment feedback', () => {
    it('pulses once for an exposed group screen edge and keeps the rigid desktop aligned', () => {
        const group: Screen[] = [
            { ...initial[0], device: 'computer' },
            { id: 'extra', device: 'computer', name: 'First', x: 800, y: 100, w: 600, h: 400, wallpaper: 2 },
            { ...initial[1], device: 'other', x: 1600 },
        ];
        const { container } = render(<Canvas screens={group} />);
        const screen = container.querySelector('.scr')!;
        const zoom = cameraStore.get()!.zoom;
        fireEvent.pointerDown(screen, { button: 0, clientX: 300, clientY: 300 });
        fireEvent.pointerMove(screen, { clientX: 300 + 195 * zoom, clientY: 300 });
        expect(native.invoke).toHaveBeenCalledExactlyOnceWith('shell_alignment_haptic');
        now = 200;
        fireEvent.pointerMove(screen, { clientX: 300 + 194 * zoom, clientY: 300 });
        expect(native.invoke).toHaveBeenCalledTimes(1);
        const [first, extra, other] = container.querySelectorAll<HTMLElement>('.scr:not(.scr--group)');
        const x = (el: HTMLElement) => Number(el.style.transform.match(/-?[\d.]+/g)![0]);
        expect(x(extra) - x(first)).toBeCloseTo(800 * zoom);
        expect(x(extra) + parseFloat(extra.style.width)).toBeCloseTo(x(other));
        fireEvent.pointerUp(screen);
        expect(native.invoke).toHaveBeenCalledTimes(1);
    });
});

describe('Shift during pointer gestures', () => {
    it.each([
        ['single', 'move'], ['single', 'resize'], ['group', 'move'], ['group', 'resize'],
    ] as const)('suppresses %s %s snapping and haptics until Shift is released', (kind, mode) => {
        const seed: Screen[] = kind === 'single' ? initial : [
            { ...initial[0], device: 'computer' },
            { ...initial[0], id: 'extra', device: 'computer', x: 800, y: 100 },
            { ...initial[1], device: 'other', x: 1800 },
        ];
        const { container } = render(<Canvas screens={seed} />);
        const display = container.querySelector('.scr')!;
        const handle = mode === 'move' ? display
            : container.querySelector(kind === 'single' ? '.scr__hz[data-c="se"]' : '.scr--group .scr__hz[data-c="se"]')!;
        const zoom = cameraStore.get()!.zoom;
        const ratio = kind === 'single' ? .625 : .375;
        fireEvent.pointerDown(handle, { button: 0, clientX: 300, clientY: 300, shiftKey: true });
        fireEvent.pointerMove(handle, {
            clientX: 300 + 195 * zoom,
            clientY: 300 + (mode === 'resize' ? 195 * ratio * zoom : 0),
            shiftKey: true,
        });
        const gap = () => {
            const screens = [...container.querySelectorAll<HTMLElement>('.scr:not(.scr--group)')];
            const x = (el: HTMLElement) => Number(el.style.transform.match(/-?[\d.]+/g)![0]);
            const neighbour = screens.pop()!;
            return x(neighbour) - Math.max(...screens.map(s => x(s) + parseFloat(s.style.width)));
        };
        expect(gap()).toBeGreaterThan(0);
        expect(native.invoke).not.toHaveBeenCalled();
        // A stationary pointer must re-snap on the next animation frame.
        fireEvent.keyUp(document, { key: 'Shift' });
        tick();
        expect(gap()).toBeCloseTo(0, 8);
        expect(native.invoke).toHaveBeenCalledExactlyOnceWith('shell_alignment_haptic');
        now = 200;
        fireEvent.keyDown(document, { key: 'Shift', shiftKey: true });
        tick();
        expect(gap()).toBeGreaterThan(0);
        expect(native.invoke).toHaveBeenCalledTimes(1);
        fireEvent.pointerUp(handle, { shiftKey: true });
        expect(native.invoke).toHaveBeenCalledTimes(1);
    });
});

it('keeps Shift keyboard fine nudging independent of pointer snap suppression', () => {
    render(<Canvas />);
    fireEvent.keyDown(document, { key: 'Shift', shiftKey: true });
    fireEvent.keyDown(document.body, { key: 'ArrowRight', shiftKey: true });
    expect(cameraStore.get()!.target.x).toBe(10);
    expect(native.invoke).not.toHaveBeenCalled();
});

it('still resolves overlap on a Shift-held release without alignment haptics', () => {
    const changed = vi.fn();
    const { container } = render(
        <Arrangement
            screens={initial}
            selectedId="one"
            onSelect={() => {}}
            onScreensChange={changed}
            settings={{ snapping: true, showHotZones: false, compact: true, padding: 48, step: 10 }}
        />,
    );
    const display = container.querySelector('.scr')!;
    const zoom = cameraStore.get()!.zoom;
    fireEvent.pointerDown(display, { button: 0, clientX: 300, clientY: 300, shiftKey: true });
    fireEvent.pointerMove(display, { clientX: 300 + 250 * zoom, clientY: 300, shiftKey: true });
    fireEvent.pointerUp(display, { shiftKey: true });
    const next = changed.mock.lastCall![0] as Screen[];
    expect(next[0].x + next[0].w).toBe(initial[1].x);
    expect(next[1]).toEqual(initial[1]);
    expect(native.invoke).not.toHaveBeenCalled();
});
