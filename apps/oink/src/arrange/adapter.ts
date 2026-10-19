import type { Layout, Placement } from '../api/types';
import type { Screen } from './geometry';
import type { Device } from '../devices/types';

// A fixed renderer scale keeps drag arithmetic precise; API values remain in the ratio plane.
const RENDER_SCALE = 4096;

/** History stores device transforms. Native geometry always comes from the live backend. */
export function renderLayout(
  layout: Layout,
  wallpapers: Record<string, string>,
  placements?: Placement[],
): Screen[] {
  return layout.geometry.screens.map((screen, index) => {
    const native = screen.display.bounds;
    const saved = placements?.find(placement => placement.device === screen.key.device);
    const origin = saved ? {
      x: saved.origin.x + native.x * saved.scale,
      y: saved.origin.y + native.y * saved.scale,
    } : screen.origin;
    const size = saved ? { w: native.w * saved.scale, h: native.h * saved.scale } : screen.size;
    const id = `${screen.key.device}:${screen.key.display}`;
    return {
      id,
      device: screen.key.device,
      name: screen.device_name,
      x: origin.x * RENDER_SCALE,
      y: origin.y * RENDER_SCALE,
      w: size.w * RENDER_SCALE,
      h: size.h * RENDER_SCALE,
      groupSize: layout.geometry.screens.filter(peer => peer.key.device === screen.key.device).length,
      wallpaper: index,
      image: wallpapers[id],
      labelWidth: native.w,
      labelHeight: native.h,
      minimumWidth: screen.size.w * RENDER_SCALE * Math.min(1, 640 / native.w),
    };
  });
}

// Resolve card presentation after history so names and connectivity stay live.
export function presentScreens(
  screens: Screen[],
    devices: ReadonlyArray<Pick<Device, 'id' | 'name' | 'group'>>,
): Screen[] {
    const cards = new Map(devices.map(device => [device.id, device]));
    return screens.map(screen => {
        const device = cards.get(screen.device ?? '');
        return { ...screen, name: device?.name ?? screen.name, offline: device?.group === 'offline' };
    });
}

/** Empty drawing input captures the backend's current group placements. */
export function layoutPlacements(layout: Layout, screens: Screen[]): Placement[] {
  const devices = new Set(layout.geometry.screens.map(screen => screen.key.device));
  return [...devices].map(device => {
    const source = layout.geometry.screens.find(screen => screen.key.device === device)!;
    const next = screens.find(screen => screen.id === `${source.key.device}:${source.key.display}`);
    const scale = (next ? next.w / RENDER_SCALE : source.size.w) / source.display.bounds.w;
    const origin = next ? { x: next.x / RENDER_SCALE, y: next.y / RENDER_SCALE } : source.origin;
    return {
      device,
      origin: {
        x: origin.x - source.display.bounds.x * scale,
        y: origin.y - source.display.bounds.y * scale,
      },
      scale,
    };
  });
}

/** Replay only current devices, keeping stored floating point transforms exact. */
export function replayPlacements(layout: Layout, stored: Placement[]): Placement[] {
  return layoutPlacements(layout, []).map(current =>
    stored.find(placement => placement.device === current.device) ?? current,
  );
}
