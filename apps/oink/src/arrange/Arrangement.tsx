import {
    membersOf,
    sameComputer,
    transformGroup,
    freeGroup,
    groupOverlaps,
    snapGroup,
    snapGroupResize,
    growGroupAvoiding,
    mergeVisibleGroups,
} from './groups';
import { tr } from '../i18n';
import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { CSSProperties, KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent, RefObject } from 'react';
import { Icon, ICONS, Kbd, VisuallyHidden } from 'oink-ui';
import { CORNERS, fitArrangement, bounds, clampMove, facingGap, cornerPoint, cornerClearance, pickRect, resizeAroundCenter, resizeFromCorner, sameRect, snap, } from './geometry';
import type { Camera, Corner, Rect, Screen, SnapEdges } from './geometry';
import { useSpringRects } from './springs';
import { useVisible } from '../window/useVisible';
import { cornerArcPath, ringBandPath, useCornerShape } from './cornerShape';
import type { CornerKs } from 'oink-ui';
import { cameraStore } from './cameraStore';
import { growArrangementView, SpringCamera, visibleArrangement } from './camera';
import { SnapFeedback } from './haptics';
import { settleGroup, type Offset } from './compaction';
import { projectScreenRect } from './renderGeometry';
export interface ArrangementSettings {
    showHotZones: boolean;
    snapping: boolean;
    compact: boolean;
    padding: number;
    step: number;
}
export type LogTone = 'neutral' | 'success' | 'warning' | 'danger' | 'info';
export interface ScreensChange {
    label: string;
    merge?: string;
}
/** One native screen moved inside its computer (system arrangement on). `drag`
 * reports the desired top-left; `end` hands over the release compaction, which the
 * owner of the backend preview applies to the snapped rectangles of the computer. */
export type ScreenMove =
    | { type: 'drag'; id: string; desired: { x: number; y: number } }
    | { type: 'end'; id: string; cancelled: true }
    | { type: 'end'; id: string; cancelled: false; label: string; compact: (rects: Rect[]) => Offset };
export interface ArrangementProps {
    screens: Screen[];
    platform?: 'mac' | 'win';
    /** Complete retained geometry, including hidden members used at release. */
    geometryScreens?: Screen[];
    onProjection?: (screens: Screen[] | null, held?: boolean) => void;
    onScreensChange: (screens: Screen[], change: ScreensChange) => void;
    selectedId: string | null;
    selectedGroup?: ReadonlySet<string>;
    /** `toggle` is Cmd/Ctrl+click, `device` a double-click; the owner applies the selection model. */
    onSelect: (id: string | null, mode?: 'toggle' | 'device') => void;
    settings: ArrangementSettings;
    onLog?: (text: string, tone?: LogTone) => void;
    keyScope?: RefObject<HTMLElement | null>;
    hintId?: string;
    notice?: {
        text: string;
    };
    contextFor?: (s: Screen) => string | undefined;
    /** Screens whose computer writes the system arrangement: one selected screen
     * moves on its own, with snapped previews from `screenPreview`. */
    screenEditable?: (s: Screen) => boolean;
    screenPreview?: Readonly<Record<string, Rect>> | null;
    onScreenMove?: (move: ScreenMove) => void;
}
const SCREEN_R = 12;
/** The selection ring: a canvas-coloured gap band, then the accent band (px outside the edge). */
const RING_GAP = 2.5, RING_OUT = 4.5;
/** A selection ring drawn as one SVG: exact corners from the first frame on every engine
 * (see ringBandPath for why it is not a spread box-shadow). */
function SelectionRing({ w, h, radii, ks }: { w: number; h: number; radii: [number, number, number, number]; ks: CornerKs }) {
    const m = Math.ceil(RING_OUT) + 1;
    return <svg className="scr__ring" aria-hidden width={w + 2 * m} height={h + 2 * m} viewBox={`${-m} ${-m} ${w + 2 * m} ${h + 2 * m}`} style={{ left: -m, top: -m }}>
      <path className="scr__ring-gap" d={ringBandPath(w, h, radii, ks, 0, RING_GAP)}/>
      <path className="scr__ring-accent" d={ringBandPath(w, h, radii, ks, RING_GAP, RING_OUT)}/>
    </svg>;
}
const HANDLE_GAP = 7;
const DRAG_SLOP = 3;
interface Interaction {
    id: string;
    group: Screen[];
    snapSuppressed: boolean;
    mode: 'move' | 'resize' | 'screen';
    corner?: Corner;
    pointerId: number;
    origin: Rect;
    grab: {
        x: number;
        y: number;
    };
    start: {
        x: number;
        y: number;
    };
    moved: boolean;
    live: Rect;
    pointer: {
        x: number;
        y: number;
    };
    wasSelected: boolean;
    /** Pressed inside a multi-computer group: a click without a move selects just this. */
    collapse?: boolean;
    view: Rect;
}
const fmt = (n: number) => String(Math.round(n)).replace('-', '−');
/** Parenthesised notes after a log sentence, e.g. "(snapped to edge)". */
const noteSuffix = (notes: string[]) => notes.length ? tr('（{0}）', notes.join(tr('；'))) : '';
export function Arrangement({ screens, platform = 'mac', geometryScreens = screens, onScreensChange, selectedId, selectedGroup, onSelect, settings, onLog, keyScope, hintId, notice, contextFor, onProjection, screenEditable, screenPreview, onScreenMove }: ArrangementProps) {
    const root = useRef<HTMLDivElement>(null);
    const visible = useVisible(root);
    const springs = useSpringRects(visible);
    const [feedback] = useState(() => new SnapFeedback());
    const [camera] = useState(() => new SpringCamera());
    const cameraViewport = useRef({ w: 0, h: 0, padding: settings.padding });
    const shape = useCornerShape();
    const [size, setSize] = useState({ w: 0, h: 0 });
    const [pixelRatio, setPixelRatio] = useState(() => globalThis.devicePixelRatio || 1);
    const [active, setActive] = useState<{
        id: string;
        mode: 'move' | 'resize' | 'screen';
        corner?: Corner;
        overlap: boolean;
    } | null>(null);
    const [announce, setAnnounce] = useState('');
    const drag = useRef<Interaction | null>(null);
    const mounted = useRef(false);
    const [lingering, setLingering] = useState(false);
    const [pressed, setPressed] = useState(false);
    const projectionTimer = useRef(0);
    const projectionShown = useRef(false);
    const projectionCallback = useRef(onProjection);
    projectionCallback.current = onProjection;
    const lingerProjection = () => {
        setLingering(true);
        clearTimeout(projectionTimer.current);
        projectionTimer.current = window.setTimeout(() => setLingering(false), 1200);
    };
    useEffect(() => () => {
        clearTimeout(projectionTimer.current);
        projectionCallback.current?.(null);
    }, []);
    useEffect(() => {
        const shift = (e: KeyboardEvent) => {
            const d = drag.current;
            if (e.key !== 'Shift' || !d)
                return;
            const suppressed = e.type === 'keydown';
            if (d.snapSuppressed === suppressed)
                return;
            d.snapSuppressed = suppressed;
            feedback.reset();
            // Modifier changes must also update a stationary mouse/trackpad drag.
            springs.wake();
        };
        document.addEventListener('keydown', shift);
        document.addEventListener('keyup', shift);
        return () => {
            document.removeEventListener('keydown', shift);
            document.removeEventListener('keyup', shift);
        };
    }, [feedback, springs]);
    useEffect(() => {
        // Native previews use the same wire arrangement DTO, but their geometry
        // comes from App's snapped backend reply rather than these rigid springs.
        const show = visible && !screenPreview && drag.current?.mode !== 'screen' && (pressed || lingering
            || (projectionShown.current && screens.some(s => springs.isMoving(s.id))));
        projectionShown.current = show;
        projectionCallback.current?.(show
            ? screens.map(s => ({ ...s, ...(springs.get(s.id) ?? s) })) : null, pressed || !!drag.current);
    });
    const log = (text: string, tone: LogTone = 'neutral') => {
        onLog?.(text, tone);
        setAnnounce(text);
    };
    useEffect(() => {
        if (notice)
            setAnnounce(notice.text);
    }, [notice]);
    useLayoutEffect(() => {
        const el = root.current!;
        const ro = new ResizeObserver(([e]) => setSize({ w: e.contentRect.width, h: e.contentRect.height }));
        ro.observe(el);
        const r = el.getBoundingClientRect();
        setSize({ w: r.width, h: r.height });
        const onResize = () => setPixelRatio(globalThis.devicePixelRatio || 1);
        window.addEventListener('resize', onResize);
        return () => { ro.disconnect(); window.removeEventListener('resize', onResize); };
    }, []);
    useLayoutEffect(() => {
        const interaction = drag.current;
        if (interaction) {
            const current = members(interaction.id);
            if (current.length !== interaction.group.length || current.some(s => {
                const initial = interaction.group.find(m => m.id === s.id);
                return !initial || !sameRect(s, initial);
            })) {
                // A native desktop change invalidates the gesture's anchor.
                // Let current backend geometry retarget all screen springs.
                drag.current = null;
                feedback.reset();
                setActive(null);
            }
        }
        const first = !mounted.current;
        mounted.current = true;
        const ids = new Set(screens.map((s) => s.id));
        for (const id of springs.ids())
            if (!id.startsWith('__') && !ids.has(id.replace(/^r:/, '')))
                springs.remove(id);
        for (const s of screens) {
            const d = drag.current;
            if (d?.moved && d.mode !== 'screen' && members(d.id).some(m => m.id === s.id))
                continue;
            // A dragged native screen follows the pointer until the first snapped
            // preview arrives; a preview holds until the committed geometry replaces it.
            const preview = screenPreview?.[s.id];
            if (d?.moved && d.mode === 'screen' && d.id === s.id && !preview)
                continue;
            const r = preview ?? pickRect(s);
            if (first)
                springs.jump(s.id, r);
            else if (!springs.has(s.id))
                springs.to(s.id, r, 'spring', { x: s.x + s.w / 2, y: s.y + s.h / 2, w: 0, h: 0 });
            else if (!sameRect(springs.target(s.id)!, r))
                springs.to(s.id, r, preview ? 'snappy' : 'soft');
        }
        const dragging = !!drag.current?.moved;
        for (const s of screens) {
            const key = 'r:' + s.id;
            const t = targetRadii(s.id);
            if (first || dragging || !springs.has(key)) {
                if (!springs.has(key) || !sameRect(springs.get(key)!, t))
                    springs.jump(key, t);
            }
            else if (!sameRect(springs.target(key)!, t)) {
                springs.to(key, t, springs.lastPreset);
            }
        }
    });
    // Window/sidebar changes fit immediately. Screen changes are followed on the
    // animation clock, starting from the live gesture camera without a release jump.
    const fit = bounds(screens.map(s => springs.get(s.id) ?? s));
    const viewportChanged = cameraViewport.current.w !== size.w || cameraViewport.current.h !== size.h
        || cameraViewport.current.padding !== settings.padding;
    const { zoom, ox, oy } = (viewportChanged && !drag.current ? fitArrangement(fit, size, settings.padding) : camera.get())
        ?? fitArrangement(fit, size, settings.padding);
    useLayoutEffect(() => {
        if (!camera.get() || (viewportChanged && !drag.current))
            camera.jump({ zoom, ox, oy });
        cameraViewport.current = { ...size, padding: settings.padding };
        springs.afterStep = (dt, elapsed) => {
            if (drag.current?.moved) return step(elapsed);
            if (drag.current) return false;
            const animated = bounds(screens.map(s => springs.get(s.id) ?? s));
            return camera.step(fitArrangement(animated, size, settings.padding), dt);
        };
    });
    useLayoutEffect(() => { springs.wake(); }, [springs, screens, size.w, size.h, settings.padding]);
    useEffect(() => {
        cameraStore.set({ zoom, fit: { ...fit }, target: bounds(screens), viewport: size });
    });
    const targetRadii = (id: string): Rect => {
        const goal = (gid: string) => springs.target(gid) ?? screens.find(s => s.id === gid)!;
        const clear = cornerClearance(goal(id), screens.filter((o) => o.id !== id).map((o) => goal(o.id)));
        const radius = (c: Corner) => (clear[c] === null ? R : Math.min(R, clear[c]! * zoom));
        return { x: radius('nw'), y: radius('ne'), w: radius('sw'), h: radius('se') };
    };
    const toVirtual = (clientX: number, clientY: number) => {
        const r = root.current!.getBoundingClientRect();
        return { x: (clientX - r.left - ox) / zoom, y: (clientY - r.top - oy) / zoom };
    };
    /** A selection spanning several computers edits as one rigid unit; anything else
     * edits its own computer. The owner keeps groups made of whole computers. */
    const grouped = !!selectedGroup && new Set(screens.filter(s => selectedGroup.has(s.id)).map(s => s.device ?? s.id)).size > 1;
    const members = (id: string) => grouped && selectedGroup!.has(id) ? screens.filter(s => selectedGroup!.has(s.id)) : membersOf(screens, id);
    const others = (id: string) => screens.filter(s => !members(id).some(m => m.id === s.id));
    /** The edited unit's bounding rectangle, carrying the pressed screen's identity. */
    const byId = (id: string): Screen | undefined => {
        const unit = members(id);
        if (!unit.length) return undefined;
        const r = bounds(unit);
        return { ...unit.find(s => s.id === id)!, ...r, minimumWidth: r.w * Math.max(...unit.map(s => (s.minimumWidth ?? 640) / s.w)) };
    };
    const unitName = (id: string) => grouped && selectedGroup!.has(id) ? tr('所选屏幕') : screens.find(s => s.id === id)?.name ?? id;
    const pointerFocus = useRef(false);
    const animate = (id: string, r: Rect, preset: 'snappy' | 'soft' | 'jump') => {
        const group = members(id);
        const from = bounds(group);
        for (const next of transformGroup(group, from, r)) {
            if (preset === 'jump')
                springs.jump(next.id, next);
            else
                springs.to(next.id, next, preset);
        }
    };
    /** A manual edit is the only compaction trigger. New props (hotplug, hidden
     * screens, connectivity, remote layouts) only retarget the springs above.
     * Only the edited computer moves; hidden screens are not in `screens`. */
    const settle = (id: string, r: Rect) => {
        const group = members(id), from = bounds(group), rest = others(id);
        const { dx, dy } = settings.compact
            ? settleGroup(transformGroup(group, from, r), rest, rest.map(s => s.device ?? s.id)) : { dx: 0, dy: 0 };
        const final = { ...r, x: r.x + dx, y: r.y + dy };
        const transformed = transformGroup(group, from, final);
        const next = mergeVisibleGroups(geometryScreens, screens.map(s => transformed.find(n => n.id === s.id) ?? s));
        return { next, final, compacted: dx !== 0 || dy !== 0 };
    };
    /** Release handling for a native screen edit: collision repair and compaction
     * translate the whole computer, from its snapped (visible) screen rectangles. */
    const compactScreens = (id: string, rects: Rect[]): Offset => {
        if (!rects.length) return { dx: 0, dy: 0 };
        const rest = others(id), from = bounds(rects);
        const free = freeGroup(rects, from, from, rest);
        const { dx, dy } = settings.compact
            ? settleGroup(transformGroup(rects, from, free), rest, rest.map(s => s.device ?? s.id)) : { dx: 0, dy: 0 };
        return { dx: free.x - from.x + dx, dy: free.y - from.y + dy };
    };
    /** Single-screen computers have no independent native arrangement to edit. */
    const editingScreen = (s: Screen) => !selectedGroup && !!onScreenMove && !!screenEditable?.(s) && membersOf(screens, s.id).length > 1;
    /** Shown selected: the single screen of a system-arranged computer, else whole units. */
    const isSelected = (s: Screen) => screenSelection && sameComputer(s, screenSelection) ? s.id === selectedId
        : (!!selectedId && members(selectedId).some(m => m.id === s.id)) || !!selectedGroup?.has(s.id);
    /** Commit a keyboard edit of one computer, including its compaction, as one change. */
    const commit = (id: string, r: Rect, change: ScreensChange) => {
        const { next, final, compacted } = settle(id, r);
        for (const screen of next) {
            if (screens.some(s => s.id === screen.id) && !sameRect(springs.target(screen.id) ?? screen, screen))
                springs.to(screen.id, pickRect(screen), 'snappy');
        }
        if (!next.every((s, i) => sameRect(s, geometryScreens[i])))
            onScreensChange(next, change);
        return { final, compacted };
    };
    const begin = (e: ReactPointerEvent, s: Screen, mode: 'move' | 'resize', corner?: Corner) => {
        if (e.button !== 0 || drag.current || zoom <= 0)
            return;
        e.stopPropagation();
        // The press focuses the screen; that focus must not re-select it plainly.
        pointerFocus.current = true;
        if (mode === 'move' && (platform === 'mac' ? e.metaKey : e.ctrlKey)) {
            onSelect(s.id, 'toggle');
            return;
        }
        if (mode === 'move' && !selectedGroup?.has(s.id) && !!onScreenMove && !!screenEditable?.(s) && membersOf(screens, s.id).length > 1) {
            // A plain press on a system-arranged computer's screen selects that screen
            // alone and moves it on its own; snapping is the backend's.
            const wasSelected = !!screenSelection && selectedId === s.id;
            if (!wasSelected) onSelect(s.id);
            // The selected screen of a system-arranged computer moves on its own;
            // its computer's other screens stay, and snapping is the backend's.
            const own = pickRect(screens.find(m => m.id === s.id)!), shown = springs.get(s.id) ?? own;
            const p = toVirtual(e.clientX, e.clientY);
            camera.jump({ zoom, ox, oy });
            feedback.reset();
            drag.current = {
                id: s.id, group: members(s.id), mode: 'screen', pointerId: e.pointerId, origin: own, snapSuppressed: false,
                grab: { x: p.x - shown.x, y: p.y - shown.y }, start: { x: e.clientX, y: e.clientY }, moved: false, live: own,
                pointer: { x: e.clientX, y: e.clientY }, wasSelected, view: visibleArrangement({ zoom, ox, oy }, size, settings.padding),
            };
            clearTimeout(projectionTimer.current);
            setPressed(true);
            onScreenMove?.({ type: 'drag', id: s.id, desired: { x: own.x, y: own.y } });
            (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
            return;
        }
        // Pressing inside a multi-computer group keeps it, so the group can be dragged.
        const collapse = mode === 'move' && grouped && !!selectedGroup?.has(s.id);
        const wasSelected = mode === 'move' && isSelected(s);
        if (!collapse && mode === 'move') onSelect(s.id);
        s = byId(s.id)!;
        const p = toVirtual(e.clientX, e.clientY);
        const shown = bounds(members(s.id).map(m => springs.get(m.id) ?? m));
        const ref = mode === 'move' ? { x: shown.x, y: shown.y } : cornerPoint(s, corner!);
        camera.jump({ zoom, ox, oy });
        const edges: SnapEdges = {};
        if (settings.snapping && !e.shiftKey) {
            if (mode === 'resize')
                snapGroupResize(members(s.id), s, s, corner!, others(s.id), 0, s.minimumWidth ?? 640, edges);
            else
                snapGroup(members(s.id), s, shown, others(s.id), 0, edges);
        }
        feedback.reset(edges);
        drag.current = {
            id: s.id, group: members(s.id), mode, corner, pointerId: e.pointerId, origin: pickRect(s),
            snapSuppressed: e.shiftKey,
            grab: { x: p.x - ref.x, y: p.y - ref.y }, start: { x: e.clientX, y: e.clientY }, moved: false, live: pickRect(s),
            pointer: { x: e.clientX, y: e.clientY }, wasSelected, collapse,
            view: visibleArrangement({ zoom, ox, oy }, size, settings.padding),
        };
        clearTimeout(projectionTimer.current);
        setPressed(true);
        (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
        if (mode === 'resize')
            startInteraction();
    };
    const startInteraction = () => {
        const d = drag.current!;
        d.moved = true;
        step();
        springs.wake();
    };
    const liveAt = (d: Interaction, cam: Camera, edges: SnapEdges): Rect => {
        const snapping = settings.snapping && !d.snapSuppressed;
        const box = root.current!.getBoundingClientRect();
        const p = { x: (d.pointer.x - box.left - cam.ox) / cam.zoom, y: (d.pointer.y - box.top - cam.oy) / cam.zoom };
        if (d.mode === 'screen') {
            // One native screen snaps to every other screen's edges, its siblings
            // included; the backend preview then keeps the computer edge-connected.
            const r = { ...d.origin, x: p.x - d.grab.x, y: p.y - d.grab.y };
            return snapping ? snap(r, screens.filter(s => s.id !== d.id), 10 / cam.zoom, edges) : r;
        }
        if (d.mode === 'resize') {
            const minimumWidth = byId(d.id)?.minimumWidth;
            const r = resizeFromCorner(d.origin, d.corner!, { x: p.x - d.grab.x, y: p.y - d.grab.y }, minimumWidth);
            const result = snapping
                ? snapGroupResize(
                    members(d.id), d.origin, r, d.corner!, others(d.id),
                    10 / cam.zoom, minimumWidth ?? 640, edges,
                ) : r;
            return members(d.id).length > 1
                ? { ...result, h: d.origin.h * result.w / d.origin.w } : result;
        }
        let r: Rect = { ...d.origin, x: p.x - d.grab.x, y: p.y - d.grab.y };
        if (snapping)
            r = snapGroup(members(d.id), d.origin, r, others(d.id), 10 / cam.zoom, edges);
        // World coordinates preserve snapped seams; only renderGeometry rounds pixels.
        return r;
    };
    const step = (dt = 0) => {
        const d = drag.current;
        if (!d?.moved)
            return false;
        let cam = camera.get()!;
        let edges: SnapEdges = {};
        let live = liveAt(d, cam, edges);
        const view = growArrangementView(d.view, live, cam.zoom, settings.padding, dt);
        const growing = view !== d.view;
        if (growing) {
            d.view = view;
            cam = fitArrangement(view, size, settings.padding);
            camera.jump(cam);
            // The pointer can be stationary while the camera changes. Reproject in
            // the same frame so the grabbed point continues to follow that pointer.
            edges = {};
            live = liveAt(d, cam, edges);
        }
        feedback.update(edges);
        if (d.mode === 'screen') {
            if (!sameRect(live, d.live)) {
                d.live = live;
                onScreenMove?.({ type: 'drag', id: d.id, desired: { x: live.x, y: live.y } });
            }
            if (!screenPreview?.[d.id])
                springs.jump(d.id, live);
            setActive((a) => (a && a.id === d.id && a.mode === 'screen' ? a : { id: d.id, mode: 'screen', overlap: false }));
            return growing;
        }
        if (!sameRect(live, d.live) || !springs.has(d.id)) {
            d.live = live;
            animate(d.id, live, 'jump');
        }
        const overlap = groupOverlaps(members(d.id), d.origin, live, others(d.id));
        setActive((a) => (a && a.id === d.id && a.overlap === overlap && a.mode === d.mode ? a : { id: d.id, mode: d.mode, corner: d.corner, overlap }));
        return growing;
    };
    const move = (e: ReactPointerEvent) => {
        const d = drag.current;
        if (!d || e.pointerId !== d.pointerId)
            return;
        if (d.snapSuppressed !== e.shiftKey) {
            d.snapSuppressed = e.shiftKey;
            feedback.reset();
        }
        d.pointer = { x: e.clientX, y: e.clientY };
        if (!d.moved) {
            if (Math.hypot(e.clientX - d.start.x, e.clientY - d.start.y) < DRAG_SLOP)
                return;
            startInteraction();
            return;
        }
        step();
        // A stationary held pointer needs more frames only while edge growth
        // changes the camera. New pointer data wakes the follower again.
        springs.wake();
    };
    const end = (e: ReactPointerEvent, cancelled = false) => {
        pointerFocus.current = false;
        const d = drag.current;
        if (!d || e.pointerId !== d.pointerId)
            return;
        drag.current = null;
        setPressed(false);
        feedback.reset();
        setActive(null);
        springs.wake();
        lingerProjection();
        if (!d.moved) {
            if (d.mode === 'screen') onScreenMove?.({ type: 'end', id: d.id, cancelled: true });
            // A click: inside a group it narrows the selection to this screen's unit;
            // on what was already selected it clears the selection.
            if (!cancelled && d.mode !== 'resize') {
                if (d.collapse) onSelect(d.id);
                else if (d.wasSelected) onSelect(null);
            }
            return;
        }
        const name = unitName(d.id);
        if (d.mode === 'screen') {
            if (cancelled || sameRect(d.live, d.origin)) {
                onScreenMove?.({ type: 'end', id: d.id, cancelled: true });
                springs.to(d.id, d.origin, 'snappy');
                return;
            }
            onScreenMove?.({ type: 'end', id: d.id, cancelled: false, label: tr('移动 {0}', name), compact: rects => compactScreens(d.id, rects) });
            return;
        }
        if (cancelled || sameRect(d.live, d.origin)) {
            animate(d.id, d.origin, 'snappy');
            return;
        }
        // Hidden screens neither block the drop nor take part in compaction.
        const free = freeGroup(members(d.id), d.origin, d.live, others(d.id));
        const { next, final, compacted } = settle(d.id, free);
        for (const screen of next) {
            if (screens.some(s => s.id === screen.id) && !sameRect(springs.target(screen.id) ?? screen, screen))
                springs.to(screen.id, pickRect(screen), 'soft');
        }
        if (next.every((s, i) => sameRect(s, geometryScreens[i])))
            return;
        onScreensChange(next, { label: d.mode === 'move' ? tr('移动 {0}', name) : tr('缩放 {0}', name) });
        const notes = [!sameRect(free, d.live) && tr('重叠，已弹到最近的空位'), compacted && tr('已靠拢')].filter((note): note is string => !!note);
        const where = noteSuffix(notes);
        log(d.mode === 'move'
            ? tr('{0} 移到 ({1}, {2}){3}', name, fmt(final.x), fmt(final.y), where) : tr('{0} 缩放到 {1} × {2}{3}', name, final.w, final.h, where), notes.length ? 'warning' : 'success');
    };
    const latest = useRef({ screens, selectedId, settings });
    latest.current = { screens, selectedId, settings };
    useEffect(() => {
        const onKey = (e: KeyboardEvent) => {
            const { screens, selectedId, settings } = latest.current;
            if (!selectedId || e.defaultPrevented || e.altKey)
                return;
            if (!['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Escape'].includes(e.key))
                return;
            const t = e.target as HTMLElement | null;
            const inScope = !t || t === document.body || (keyScope?.current?.contains(t) ?? false);
            if (!inScope || (t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))))
                return;
            if (t && t.tagName === 'BUTTON')
                return;
            if (drag.current)
                return;
            const unit = byId(selectedId);
            if (!unit)
                return;
            // Log and label with the unit's name: a computer, or the selected group.
            const s = { ...unit, name: unitName(unit.id) };
            if (e.key === 'Escape') {
                e.preventDefault();
                onSelect(null);
                (document.activeElement as HTMLElement | null)?.blur?.();
                return;
            }
            e.preventDefault();
            const rest = others(s.id);
            if (e.metaKey || e.ctrlKey) {
                const dir = e.key === 'ArrowUp' || e.key === 'ArrowRight' ? 1 : -1;
                const factor = e.shiftKey ? 1.02 : 1.1;
                const w = dir > 0 ? s.w * factor : s.w / factor;
                const sized = resizeAroundCenter(s, w, s.minimumWidth);
                const r = growGroupAvoiding(members(s.id), s, sized.w, rest, s.minimumWidth);
                if (sized.w === s.w || !r) {
                    onLog?.(dir > 0 ? tr('{0} 无法再放大，忽略按键', s.name) : tr('{0} 无法再缩小，忽略按键', s.name));
                    return;
                }
                lingerProjection();
                const { compacted } = commit(s.id, r, { label: tr('缩放 {0}', s.name), merge: `key-resize:${s.id}` });
                log(compacted ? tr('{0} 缩放到 {1} × {2}{3}', s.name, r.w, r.h, noteSuffix([tr('已靠拢')])) : tr('{0} 缩放到 {1} × {2}', s.name, r.w, r.h));
                return;
            }
            const axis = e.key === 'ArrowLeft' || e.key === 'ArrowRight' ? 'x' : 'y';
            const dir = e.key === 'ArrowRight' || e.key === 'ArrowDown' ? 1 : -1;
            const want = dir * settings.step * (e.shiftKey ? 1 : 10);
            const own = screens.find(m => m.id === selectedId);
            if (own && editingScreen(own)) {
                // Same preview/commit path as a drag: the backend snaps the native screen.
                const at = springs.target(own.id) ?? own;
                onScreenMove?.({ type: 'drag', id: own.id, desired: { x: at.x + (axis === 'x' ? want : 0), y: at.y + (axis === 'y' ? want : 0) } });
                onScreenMove?.({ type: 'end', id: own.id, cancelled: false, label: tr('移动 {0}', own.name), compact: rects => compactScreens(own.id, rects) });
                lingerProjection();
                return;
            }
            const got = members(s.id).reduce((got, m) => {
                const allowed = clampMove(m, rest, axis, want);
                return Math.abs(allowed) < Math.abs(got) ? allowed : got;
            }, want);
            if (got === 0) {
                onLog?.(tr('{0} 被相邻屏幕挡住，忽略按键', s.name));
                return;
            }
            const r = { ...pickRect(s), [axis]: s[axis] + got };
            if (settings.snapping && facingGap(r, rest, axis, dir) === 0)
                feedback.pulse();
            lingerProjection();
            const { final, compacted } = commit(s.id, r, { label: tr('移动 {0}', s.name), merge: `key-move:${s.id}` });
            const notes = [got !== want && tr('已贴边'), compacted && tr('已靠拢')].filter((note): note is string => !!note);
            log(tr('{0} 移到 ({1}, {2}){3}', s.name, fmt(final.x), fmt(final.y), noteSuffix(notes)));
        };
        document.addEventListener('keydown', onKey);
        return () => document.removeEventListener('keydown', onKey);
    });
    const R = SCREEN_R * shape.rk;
    const gap = HANDLE_GAP * shape.rk;
    const M = gap + 6;
    const onRootPointerDown = (e: ReactPointerEvent) => {
        if (e.target === root.current)
            onSelect(null);
    };
    const onScreenKeyDown = (e: ReactKeyboardEvent, s: Screen) => {
        if ((e.key === 'Enter' || e.key === ' ') && selectedId !== s.id) {
            e.preventDefault();
            onSelect(s.id);
        }
    };
    const paintOrigin = root.current?.getBoundingClientRect() ?? { x: 0, y: 0 };
    // Use only existing top-left whitespace. Include selection arcs/handles in
    // the exclusion margin, and keep the toolbar's top-right footprint clear.
    // Recalculate from painted springs so zoom and animation cannot cover hints.
    const toolbar = keyScope?.current?.querySelector('.canvas-tools')?.getBoundingClientRect();
    const hintRects = screens.map(s => springs.get(s.id) ?? s);
    for (const s of screens) {
        if ((s.groupSize ?? members(s.id).length) > 1
            && members(s.id).some(m => m.id === selectedId || selectedGroup?.has(m.id)))
            hintRects.push(bounds(members(s.id).map(m => springs.get(m.id) ?? m)));
    }
    const hintWidth = hintRects.reduce((width, rect) => {
        const r = projectScreenRect(rect, { zoom, ox, oy }, pixelRatio, paintOrigin);
        return r.y - M - 8 < 38 && r.y + r.h + M + 8 > 10
            ? Math.min(width, Math.max(0, r.x - M - 8 - 12)) : width;
    }, Math.max(0, (toolbar ? toolbar.left - paintOrigin.x - 8 : size.w - 12) - 12));
    // Yield to an actual move/resize gesture (`active`), not to every press: a
    // click that switches the selection must keep the same hint mounted.
    const showHints = screens.some(s => s.id === selectedId) && !active && hintWidth >= 180 && size.h >= 48;
    // A selected screen of a system-arranged computer is selected on its own:
    // solid ring on the screen, dashed bounding rectangle for its computer.
    const selectedScreen = screens.find(m => m.id === selectedId);
    const screenSelection = selectedScreen && editingScreen(selectedScreen) ? selectedScreen : undefined;
    return (<div ref={root} className="arr" data-hotzones={settings.showHotZones ? '' : undefined} data-interacting={active ? active.mode : undefined} data-corner={active?.corner} onPointerDown={onRootPointerDown}>
      {screens.map((s) => {
            const r = springs.get(s.id) ?? s;
            const ra = springs.get('r:' + s.id) ?? { x: R, y: R, w: R, h: R };
            const clampR = (v: number) => Math.max(0, Math.min(R, v));
            const rc = { nw: clampR(ra.x), ne: clampR(ra.y), sw: clampR(ra.w), se: clampR(ra.h) };
            const painted = projectScreenRect(r, { zoom, ox, oy }, pixelRatio, paintOrigin);
            const W = painted.w;
            const H = painted.h;
            const isActive = !!active && (active.mode === 'screen' ? active.id === s.id : members(active.id).some(m => m.id === s.id));
            const alone = !!screenSelection && sameComputer(s, screenSelection);
            const selected = isSelected(s);
            // Group members are outlined and resized only by the group's rectangle.
            const inGroup = grouped && !!selectedGroup?.has(s.id);
            const multi = (s.groupSize ?? membersOf(screens, s.id).length) > 1 || inGroup;
            const style = {
                transform: `translate(${painted.x}px, ${painted.y}px)`,
                width: W, height: H,
                zIndex: isActive ? 3 : selected ? 2 : 1,
            } as CSSProperties;
            const hz = Math.max(14, Math.min(R * 1.2, W / 3, H / 3));
            const out = gap + 8;
            const label = tr('{0}，{1} × {2}，位置 {3}, {4}', s.name, fmt(s.w), fmt(s.h), fmt(s.x), fmt(s.y));
            return (<div key={s.id} className="scr" style={style} role="button" tabIndex={0} aria-pressed={selected} aria-label={s.offline ? tr('{0}，离线', label) : label} aria-describedby={hintId} data-selected={selected && (!multi || alone) ? '' : undefined} data-active={isActive ? active!.mode : undefined} data-corner={isActive ? active!.corner : undefined} data-overlap={isActive && active!.overlap ? '' : undefined} data-ctx={contextFor?.(s)} onContextMenu={() => { if (contextFor?.(s))
                onSelect(s.id); }} onFocus={() => { if (!pointerFocus.current) onSelect(s.id); }} onDoubleClick={() => onSelect(s.id, 'device')} onKeyDown={(e) => onScreenKeyDown(e, s)} onPointerDown={(e) => begin(e, s, 'move')} onPointerMove={move} onPointerUp={(e) => end(e)} onPointerCancel={(e) => end(e, true)} onLostPointerCapture={(e) => end(e)}>
            {/* Write radii on the painted node: the WebKit fallback rereads its
                geometry on style mutations, including animated contact corners. */}
            <div className="scr__body" data-offline={s.offline ? '' : undefined} style={{
                '--r-nw': `${rc.nw}px`, '--r-ne': `${rc.ne}px`,
                '--r-sw': `${rc.sw}px`, '--r-se': `${rc.se}px`,
                backgroundImage: s.image && !s.offline ? `url(${s.image})` : undefined,
            } as CSSProperties}>
              <span className="scr__content">
              {s.offline && <Icon className="scr__offline">
                <g dangerouslySetInnerHTML={{ __html: ICONS.monitor }}/>
                <path d="M3 3 17 17"/>
              </Icon>}
              <span className="scr__label">
                <span className="scr__name">{s.name}</span>
                <span className="scr__res">{s.labelWidth ?? Math.round(r.w)} × {s.labelHeight ?? Math.round(r.h)}</span>
              </span>
              </span>
            </div>
            {!multi && CORNERS.map((c) => (<div key={c} className="scr__hz" data-c={c} aria-hidden style={{
                        width: hz + out, height: hz + out,
                        [c[1] === 'e' ? 'right' : 'left']: -out,
                        [c[0] === 's' ? 'bottom' : 'top']: -out,
                    } as CSSProperties} onPointerDown={(e) => begin(e, s, 'resize', c)}/>))}
            {selected && (!multi || alone) && <SelectionRing w={W} h={H} radii={[rc.nw, rc.ne, rc.se, rc.sw]} ks={shape.ks}/>}
            {!multi && <svg className="scr__arcs" aria-hidden width={W + 2 * M} height={H + 2 * M} viewBox={`${-M} ${-M} ${W + 2 * M} ${H + 2 * M}`} style={{ left: -M, top: -M }}>
              {CORNERS.map((c) => {
                    const d = cornerArcPath(c, W, H, gap, rc[c] + gap, shape.n);
                    return <path key={c} className="scr__arc" data-c={c} d={d} pathLength={1}/>;
                })}
            </svg>}
          </div>);
        })}
      {(grouped ? screens.filter(s => selectedGroup!.has(s.id)).slice(0, 1) : screens.filter((s, i) =>
            (s.groupSize ?? members(s.id).length) > 1
            && screens.findIndex(m => sameComputer(s, m)) === i
            && members(s.id).some(m => m.id === selectedId || selectedGroup?.has(m.id)),
        )).map(s => {
            const r = bounds(members(s.id).map(m => springs.get(m.id) ?? m));
            const painted = projectScreenRect(r, { zoom, ox, oy }, pixelRatio, paintOrigin);
            const W = painted.w;
            const H = painted.h;
            const out = gap + 8;
            const hz = 14;
            const activeScreen = active && active.mode !== 'screen' ? screens.find(m => m.id === active.id) : undefined;
            const isActive = !!activeScreen && members(s.id).some(m => m.id === activeScreen.id);
            return (
                <div
                    key={grouped ? 'group:selection' : `group:${s.device}`}
                    className="scr scr--group"
                    data-selected=""
                    data-group={grouped ? 'selection' : s.device}
                    data-screen-selected={screenSelection && sameComputer(s, screenSelection) ? '' : undefined}
                    data-active={isActive ? active!.mode : undefined}
                    data-corner={isActive ? active!.corner : undefined}
                    style={{
                        transform: `translate(${painted.x}px,${painted.y}px)`,
                        width: W,
                        height: H,
                        zIndex: 4,
                    } as CSSProperties}
                    onPointerMove={move}
                    onPointerUp={e => end(e)}
                    onPointerCancel={e => end(e, true)}
                    onLostPointerCapture={e => end(e)}
                >
                    <div className="scr__group-border">{!(screenSelection && sameComputer(s, screenSelection)) && <SelectionRing w={W} h={H} radii={[R, R, R, R]} ks={shape.ks}/>}</div>
                    {CORNERS.map(c => (
                        <div
                            key={c}
                            className="scr__hz"
                            data-c={c}
                            aria-hidden
                            style={{
                                width: hz + out,
                                height: hz + out,
                                [c[1] === 'e' ? 'right' : 'left']: -out,
                                [c[0] === 's' ? 'bottom' : 'top']: -out,
                            } as CSSProperties}
                            onPointerDown={e => begin(e, s, 'resize', c)}
                        />
                    ))}
                    <svg
                        className="scr__arcs"
                        aria-hidden
                        width={W + 2 * M}
                        height={H + 2 * M}
                        viewBox={`${-M} ${-M} ${W + 2 * M} ${H + 2 * M}`}
                        style={{ left: -M, top: -M }}
                    >
                        {CORNERS.map(c => (
                            <path
                                key={c}
                                className="scr__arc"
                                data-c={c}
                                d={cornerArcPath(c, W, H, gap, R + gap, shape.n)}
                                pathLength={1}
                            />
                        ))}
                    </svg>
                </div>
            );
        })}
      {/* Inside a multi-computer group, each member gets a dashed accent outline: per
          screen where the computer writes its system arrangement, otherwise one around
          the computer's bounds, which edits as one big screen. */}
      {grouped && [...new Set(screens.filter(s => selectedGroup!.has(s.id)).map(s => s.device ?? s.id))].flatMap(device => {
            const own = screens.filter(s => (s.device ?? s.id) === device);
            return screenEditable?.(own[0]) && own.length > 1 ? own.map(s => ({ key: s.id, rects: [s] })) : [{ key: device, rects: own }];
        }).map(({ key, rects }) => {
            const painted = projectScreenRect(bounds(rects.map(m => springs.get(m.id) ?? m)), { zoom, ox, oy }, pixelRatio, paintOrigin);
            return <div key={`outline:${key}`} className="scr-outline" data-outline={key} aria-hidden
                style={{ transform: `translate(${painted.x}px,${painted.y}px)`, width: painted.w, height: painted.h }}/>;
        })}
      {showHints && <div className="arr__keys" style={{ width: hintWidth }} role="group" aria-label={tr('排列快捷键')}>
        <div className="arr__keys-row">
          <span className="arr__key-hint" title={tr('移动')}>
            <span className="arr__arrows">{['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight'].map(k => <Kbd key={k} k={k} size="sm"/>)}</span>
            <span className="arr__key-label">{tr('移动')}</span>
          </span>
          <span className="arr__key-sep" aria-hidden>·</span>
          <span className="arr__key-hint" title={tr('缩放')}>
            {platform === 'mac' ? <Kbd k="Meta" size="sm"/> : <Kbd size="sm">Ctrl</Kbd>}<span className="arr__key-label">{tr('缩放')}</span>
          </span>
          <span className="arr__key-sep" aria-hidden>·</span>
          <span className="arr__key-hint" title={tr('微调')}>
            <Kbd k="Shift" size="sm"/><span className="arr__key-label">{tr('微调')}</span>
          </span>
        </div>
      </div>}
      {screens.length === 0 && <p className="arr__empty">{tr('没有可共享的屏幕')}</p>}
      <VisuallyHidden aria-live="polite">{announce}</VisuallyHidden>
    </div>);
}
