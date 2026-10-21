import type { Layout } from '../api/types';

/**
 * A real three-computer layout, reduced to geometry: a hidden and disabled
 * Windows 4K screen (a), a Mac (b) with an external display above and to the
 * right of its built-in screen, and a second Mac (c) touching b's built-in
 * screen with a vertical gap under the external display.
 */
const screen = (device: string, display: string, os: 'macos' | 'windows', bounds: { x: number; y: number; w: number; h: number },
    origin: { x: number; y: number }, size: { w: number; h: number }, off = false) => ({
    key: { device: device.repeat(64), display }, device_name: `${os}-${device}`, os,
    display: { key: display, name: `display-${display}`, bounds, pixel_width: bounds.w * 2, pixel_height: bounds.h * 2,
        reference_extent: null, rotation_deg: 0, is_primary: display === '0', is_builtin: os === 'macos' && display === '0', is_virtual: false },
    origin, size, size_source: 'manual' as const, gain: 1, hidden: off, device_disabled: off, online: true,
});

export const threeComputers: Layout = {
    revision: 1, source: 'manual',
    geometry: {
        bounds: { x: 0.7964601283373164, y: -0.07206373775138236, w: 1.0105985099466683, h: 0.671349180967229 },
        screens: [
            screen('a', '0', 'windows', { x: 0, y: 0, w: 3840, h: 2160 },
                { x: 1.2373427733092064, y: -0.4917414721263822 }, { w: 0.7460937499999998, h: 0.41967773437499983 }, true),
            screen('b', '0', 'macos', { x: 0, y: 0, w: 1728, h: 1117 },
                { x: 0.7964601283373164, y: 0.241570282576894 }, { w: 0.5018144325252422, h: 0.3243788895432266 }),
            screen('b', '1', 'macos', { x: 1560, y: -1080, w: 1920, h: 1080 },
                { x: 1.2494870465892711, y: -0.07206373775138236 }, { w: 0.5575715916947135, h: 0.31363402032827636 }),
            screen('c', '0', 'macos', { x: 0, y: 0, w: 1728, h: 1117 },
                { x: 1.2982745608625585, y: 0.2749065536726199 }, { w: 0.5018144325252422, h: 0.3243788895432266 }),
        ],
    },
};
