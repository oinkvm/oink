import { expect, it } from 'vitest';
import type { ArrangementSnapshot } from '../api/layoutProfiles';
import { snapshotFromScreens } from './compactionEdit';
import { RENDER_SCALE, renderLayout } from './adapter';
import { compactScreens } from './compaction';
import { cornerClearance, type Screen } from './geometry';
import type { Layout } from '../api/types';

const before: ArrangementSnapshot = {
    placements: [{ device: 'a', origin: { x: 2, y: 3 }, scale: .001 }],
    native: [{ device: 'a', token: 'native', primary: { device: 'a', display: 'left' }, screens: [
        { screen: { device: 'a', display: 'left' }, origin: { x: -10, y: 5 }, size: { w: 10, h: 10 } },
        { screen: { device: 'a', display: 'right' }, origin: { x: 0, y: 0 }, size: { w: 10, h: 10 } },
    ] }],
    disabled: [], active_profile_id: 'profile',
    topology: { gap_travel: true, overlap_tolerance: 0, minimum_portal: 0, portal_snap: 0, walls: true, endpoint_reach: 0 },
};
const screens: Screen[] = before.native[0].screens.map(s => ({ id: `a:${s.screen.display}`, device: 'a', name: s.screen.display, wallpaper: 0,
    x: (2 + s.origin.x * .001) * RENDER_SCALE, y: (3 + s.origin.y * .001) * RENDER_SCALE,
    w: s.size.w * .001 * RENDER_SCALE, h: s.size.h * .001 * RENDER_SCALE }));

it('preserves the native-zero anchor while resizing a rigid desktop with negative native origins', () => {
    const next = screens.map(s => ({ ...s, x: s.x * 2, y: s.y * 2, w: s.w * 2, h: s.h * 2 }));
    const result = snapshotFromScreens(before, screens, next);
    expect(result.native).toEqual(before.native);
    expect(result.placements[0].origin.x).toBeCloseTo(4, 12);
    expect(result.placements[0].origin.y).toBeCloseTo(6, 12);
    expect(result.placements[0].scale).toBe(.002);
});

it('returns exactly the same arrangement for unchanged geometry', () => {
    expect(snapshotFromScreens(before, screens, screens)).toEqual(before);
});

it('translates a desktop after a native screen edit without rewriting any native origins', () => {
    const next = screens.map(s => ({ ...s, x: s.x + 30, y: s.y - 20 }));
    const result = snapshotFromScreens(before, screens, next);
    expect(result.native).toBe(before.native);
    expect(result.placements[0].scale).toBe(before.placements[0].scale);
    expect(result.placements[0].origin.x).toBeCloseTo(2 + 30 / RENDER_SCALE, 12);
    expect(result.placements[0].origin.y).toBeCloseTo(3 - 20 / RENDER_SCALE, 12);
});

it('keeps compacted contact exact through the placement commit and render round trip', () => {
    const scale = .5018144325252422 / 1728;
    const snapshot: ArrangementSnapshot = {
        ...before,
        placements: [
            { device: 'a', origin: { x: 45.04925694126924, y: 24.03391521322931 }, scale },
            { device: 'b', origin: { x: .5163916920004609, y: 24.065 }, scale },
        ],
        native: ['a', 'b'].map(device => ({ device, token: 'native', primary: { device, display: 'main' }, screens: [
            { screen: { device, display: 'main' }, origin: { x: 0, y: 0 }, size: { w: 1728, h: 1117 } },
        ] })),
    };
    const layout: Layout = { revision: 1, source: 'manual', geometry: { bounds: { x: 0, y: 0, w: 46, h: 25 },
        screens: snapshot.native.map(n => ({ key: n.primary, device_name: n.device, os: 'macos',
            display: { key: 'main', name: 'Display', bounds: { x: 0, y: 0, w: 1728, h: 1117 },
                pixel_width: 3456, pixel_height: 2234, reference_extent: null, rotation_deg: 0,
                is_primary: true, is_builtin: true, is_virtual: false },
            origin: snapshot.placements.find(p => p.device === n.device)!.origin,
            size: { w: 1728 * scale, h: 1117 * scale }, size_source: 'manual', gain: 1,
            hidden: false, device_disabled: false, online: true })),
    } };
    const old = renderLayout(layout, {}, snapshot.placements), compacted = compactScreens(old, old);
    const committed = snapshotFromScreens(snapshot, old, compacted);
    const [right, left] = renderLayout(layout, {}, committed.placements);
    expect(right.x).toBe(left.x + left.w);
    expect(cornerClearance(left, [right]).ne).toBe(0);
    expect(cornerClearance(right, [left]).sw).toBe(0);
    expect(committed.native).toBe(snapshot.native);
    expect(committed.placements.map(p => p.scale)).toEqual(snapshot.placements.map(p => p.scale));
});
