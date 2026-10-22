import type { Layout } from '../api/types';

/** Reported pre-compaction geometry: two equal Mac screens far apart diagonally.
 * Device/display identities are placeholders; coordinates and sizes are retained. */
const screen = (device: string, x: number, y: number) => ({
    key: { device: device.repeat(64), display: 'main' }, device_name: `Mac ${device}`, os: 'macos' as const,
    display: { key: 'main', name: 'Built-in Display', bounds: { x: 0, y: 0, w: 1728, h: 1117 },
        pixel_width: 3456, pixel_height: 2234, reference_extent: { w: .5018144325252422, h: .32437888954322663 },
        rotation_deg: 0, is_primary: true, is_builtin: true, is_virtual: false },
    origin: { x, y }, size: { w: .5018144325252422, h: .3243788895432266 }, size_source: 'manual' as const,
    gain: 1, hidden: false, device_disabled: false, online: true,
});

export const diagonalComputers: Layout = {
    revision: 14, source: 'manual', geometry: {
        bounds: { x: .5163916920004609, y: 0, w: 45.034679681794024, h: 24.358294102772536 },
        screens: [screen('b', 45.04925694126924, 24.03391521322931), screen('a', .5163916920004609, 0)],
    },
};
