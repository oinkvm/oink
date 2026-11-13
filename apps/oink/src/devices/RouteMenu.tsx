import { useCallback, useEffect, useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';
import { MenuRadioGroup, MenuRadioItem, MenuSub, Tooltip, toast } from 'oink-ui';
import { ApiFailure, api } from '../api/client';
import { agent } from '../api/store';
import type { MemberRoutes, RouteRow, RouteSelection } from '../api/types';
import { problemText, tr } from '../i18n';
import { latencyNumber, latencyTone, type LatencyTone } from './latency';
import type { Device } from './types';

/** IPv4 octets compared as numbers: 10.0.0.9 < 10.0.0.10, 192.168.1.20 < 192.168.1.100 (design §6.3). */
export function compareAddress(a: string, b: string): number {
    const key = (ip: string) => ip.split('.').map(Number);
    const left = key(a), right = key(b);
    for (let i = 0; i < Math.max(left.length, right.length); i++) {
        const diff = (left[i] ?? -1) - (right[i] ?? -1);
        if (diff) return diff;
    }
    return 0;
}

/** Reachable and measuring rows first, unreachable after; each group in address order. */
export function orderRows(rows: RouteRow[]): RouteRow[] {
    const group = (row: RouteRow) => row.state === 'unreachable' ? 1 : 0;
    return [...rows].sort((a, b) => group(a) - group(b) || compareAddress(a.ip, b.ip));
}

/** A latency cell: whole milliseconds coloured like the device list; no measurement is grey. */
export interface Latency { text: string; tone: LatencyTone | 'none' }

/** Latency per design §6.4: whole ms (device-list rounding, at most 999). */
export function formatRouteLatency(row: Pick<RouteRow, 'state' | 'rtt_ms'>): Latency {
    if (row.state === 'unreachable') return { text: tr('无响应'), tone: 'none' };
    if (row.rtt_ms === null) return { text: tr('测量中…'), tone: 'none' };
    return { text: `${latencyNumber(row.rtt_ms)} ms`, tone: latencyTone(row.rtt_ms) ?? 'none' };
}

const value = (selection: RouteSelection) => selection.type === 'address' ? `ip:${selection.ip}` : selection.type;

export interface RouteItems {
    value: string;
    /** First row: "Relay" only while relayed. */
    top: { kind: 'relay'; hint: string | null; latency: Latency | null } | null;
    auto: { disabled: boolean; hint: string | null; tip: string | null; latency: Latency | null };
    rows: { ip: string; disabled: boolean; latency: Latency; tip: string | null }[];
}

/** Everything the submenu shows, from one route snapshot (design §6.1). */
export function routeItems(routes: MemberRoutes, peer: string): RouteItems {
    const relayed = routes.relayed;
    const passive = !relayed && routes.passive;
    const noOwn = passive && !routes.can_replace;
    const ownTip = noOwn ? tr('无法从本机连接到 {0}', peer) : null;
    const selected = relayed ? 'relay' : passive || routes.selection_unavailable ? 'auto' : value(routes.selection);
    // The selected automatic or relay row shows the current link latency.
    const current = (row: string) => selected === row
        ? formatRouteLatency({ state: 'reachable', rtt_ms: routes.current_rtt_ms })
        : null;
    const inUse = routes.rows.find(row => row.current);
    // A withdrawn address still has a remembered choice; keep its disabled
    // row until facts publish it again or a click clears/replaces the choice.
    const rows: RouteRow[] = routes.remembered_ip && !routes.rows.some(row => row.ip === routes.remembered_ip)
        ? [...routes.rows, { ip: routes.remembered_ip, port: 0, state: 'unreachable', rtt_ms: null, current: false }]
        : routes.rows;
    return {
        value: selected,
        top: relayed
            ? { kind: 'relay', hint: routes.relay_via_name ? tr('经 {0}', routes.relay_via_name) : null, latency: current('relay') }
            : null,
        auto: {
            disabled: false,
            hint: selected !== 'auto' ? null : passive && routes.peer_ip
                ? tr('{0}（{1}） 连入', peer, routes.peer_ip) : inUse?.ip ?? null,
            tip: null,
            latency: current('auto'),
        },
        rows: orderRows(rows).map(row => ({
            ip: row.ip,
            // Keep an unavailable remembered target visible, without selecting it.
            disabled: noOwn || row.state === 'unreachable' || (routes.selection_unavailable && row.ip === routes.remembered_ip),
            latency: formatRouteLatency(row),
            tip: ownTip,
        })),
    };
}

function useRoutes(device: string): MemberRoutes | undefined {
    const subscribe = useCallback((changed: () => void) => agent.routes.subscribe(device, changed), [device]);
    const read = useCallback(() => agent.routes.get(device), [device]);
    return useSyncExternalStore(subscribe, read);
}

function Tip({ tip, children }: { tip: string | null; children: ReactNode }) {
    // Menu items stay hoverable when aria-disabled; the wrapper hosts the tooltip.
    return tip ? <Tooltip interactive={false} content={tip}><div className="devmenu__tipwrap">{children}</div></Tooltip> : <>{children}</>;
}

function Label({ text, hint }: { text: ReactNode; hint: string | null }) {
    return <span className="devmenu__route"><span>{text}</span>{hint && <span className="devmenu__route-hint">{hint}</span>}</span>;
}

function Ms({ latency }: { latency: Latency | null }) {
    return latency && <span className="devmenu__route-ms" data-tone={latency.tone}>{latency.text}</span>;
}

/** Mounted only while the submenu is open; it reads the latest background result. */
function RouteItemsView({ device, peer }: { device: string; peer: string }) {
    const routes = useRoutes(device);
    useEffect(() => { void api.memberRoutes(device).then(agent.routes.adopt, () => undefined); }, [device]);
    if (!routes) return null;
    const items = routeItems(routes, peer);
    const choose = (next: string) => {
        if (next === 'relay') return;
        const selection: RouteSelection = next === 'auto' ? { type: 'auto' } : { type: 'address', ip: next.slice(3) };
        // The menu has closed by now: a refusal or failure needs its own notice.
        // Either way the stored view is read again, so the radio shows what is in use.
        void api.setMemberRoute(device, selection)
            .then(applied => {
                if (!applied) toast.danger({ title: tr('无法切换线路'), description: tr('这条线路现在不可用') });
            }, error => {
                toast.danger({ title: tr('无法切换线路'), description: problemText(error instanceof ApiFailure ? error.problem.code : 'backend_failed') });
            })
            .then(() => api.memberRoutes(device))
            .then(agent.routes.adopt, () => undefined);
    };
    const top = items.top;
    return <MenuRadioGroup aria-label={tr('选择线路')} value={items.value}>
        {top && <MenuRadioItem className="devmenu__route-item" value={top.kind} closeOnSelect shortcut={<Ms latency={top.latency}/>}>
            <Label text={tr('中转')} hint={top.hint}/>
        </MenuRadioItem>}
        <Tip tip={items.auto.tip}>
            <MenuRadioItem className="devmenu__route-item" value="auto" onSelect={() => choose('auto')} closeOnSelect disabled={items.auto.disabled} shortcut={<Ms latency={items.auto.latency}/>}>
                <Label text={tr('自动')} hint={items.auto.hint}/>
            </MenuRadioItem>
        </Tip>
        {items.rows.map(row => <Tip key={row.ip} tip={row.tip}>
            <MenuRadioItem className="devmenu__route-item" value={`ip:${row.ip}`} onSelect={() => choose(`ip:${row.ip}`)} closeOnSelect disabled={row.disabled} shortcut={<Ms latency={row.latency}/>}>
                <span className="devmenu__route-ip">{row.ip}</span>
            </MenuRadioItem>
        </Tip>)}
    </MenuRadioGroup>;
}

/** The online-device "选择线路" submenu (design §6.1, §6.2). */
export function RouteMenu({ d }: { d: Device }) {
    return <MenuSub label={tr('选择线路')} icon="link">
        <RouteItemsView device={d.id} peer={d.name}/>
    </MenuSub>;
}

/** Card latency tooltip line (design Q13): 直连 · <route> · <latency>. */
export function routeSummary(routes: MemberRoutes | undefined, peer: string): string | null {
    if (!routes?.available || routes.relayed) return null;
    const current = routes.rows.find(row => row.current);
    const route = routes.passive
        ? routes.peer_ip ? tr('{0}（{1}） 连入', peer, routes.peer_ip) : null
        : current?.ip;
    if (!route) return null;
    const latency = routes.current_rtt_ms === null ? null : formatRouteLatency({ state: 'reachable', rtt_ms: routes.current_rtt_ms }).text;
    return [tr('直连'), route, latency].filter(Boolean).join(' · ');
}
export { useRoutes };
