import { act } from '@testing-library/react';
import { createLayoutProfilesFixture } from '../api/layoutProfiles.fixture';
import type { ArrangementSnapshot, DeviceArrangementState } from '../api/layoutProfiles';
import { agent } from '../api/store';
import type { Transport } from '../api/transport';
import type { Layout, Placement } from '../api/types';

/** Test support only: the contract fixture seeded from a plain layout and wired into
 * the app store, recording every request the UI sends. */
export function snapshotOf(layout: Layout, active = '00000000000000000000000000000001'): ArrangementSnapshot {
  const devices = [...new Set(layout.geometry.screens.map(s => s.key.device))];
  return {
    active_profile_id: active,
    placements: devices.map(device => {
      const s = layout.geometry.screens.find(s => s.key.device === device)!, scale = s.size.w / s.display.bounds.w;
      return { device, origin: { x: s.origin.x - s.display.bounds.x * scale, y: s.origin.y - s.display.bounds.y * scale }, scale };
    }),
    disabled: layout.geometry.screens.map(s => ({ screen: s.key, disabled: s.hidden })),
    screen_shared: devices.map(device => ({ device, shared: !layout.geometry.screens.find(s => s.key.device === device)!.device_disabled })),
    native: devices.map(device => {
      const own = layout.geometry.screens.filter(s => s.key.device === device);
      return { device, token: `native:${device}:1`, primary: (own.find(s => s.display.is_primary) ?? own[0]).key,
        screens: own.map(s => ({ screen: s.key, origin: { x: s.display.bounds.x, y: s.display.bounds.y }, size: { w: s.display.bounds.w, h: s.display.bounds.h } })) };
    }),
    topology: { gap_travel: true, walls: true, overlap_tolerance: 0, minimum_portal: .01, portal_snap: 0, endpoint_reach: 0 },
  };
}

const fallback: Transport = {
  call: async <R>({ method }: { method: string }) => (method === 'group' ? { members: [] } : []) as R,
  shell: async <R>() => null as R,
  onEvent: async () => () => {},
};

export interface Request { method: string; params?: Record<string, unknown>; reply?: unknown }
export function profileHarness(options: { layout?: Layout; snapshot?: ArrangementSnapshot; devices?: DeviceArrangementState[] }) {
  const fixture = createLayoutProfilesFixture({ snapshot: options.snapshot ?? snapshotOf(options.layout!), devices: options.devices, fallback });
  const requests: Request[] = [];
  let stop = () => {};
  void fixture.transport.onEvent(agent.receive).then(off => { stop = off; });
  agent.adopt('layoutProfiles', fixture.state());
  const of = (method: string) => requests.filter(r => r.method === method);
  return {
    fixture, requests, of,
    call: async <R>(request: Request) => {
      const sent: Request = structuredClone(request);
      requests.push(sent);
      const reply = await fixture.transport.call<R>(request);
      sent.reply = structuredClone(reply);
      return reply;
    },
    /** Placements the UI committed: edits first, then replay targets, in request order. */
    placements: () => requests.flatMap(r => r.method === 'commit_arrangement' ? [r.params!.placements as Placement[]]
      : r.method === 'replay_arrangement' ? [(r.params!.target as ArrangementSnapshot).placements] : []),
    /** Let simulated owner work finish and the UI adopt its outcome. */
    settle: async () => { await act(async () => { for (let i = 0; i < 4; i++) { await fixture.flush(); await new Promise(r => setTimeout(r, 0)); } }); },
    stop: () => stop(),
  };
}
