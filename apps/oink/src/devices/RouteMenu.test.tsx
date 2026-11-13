// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { readFileSync } from 'node:fs';
import { afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { OinkProvider, Toaster, toast } from 'oink-ui';
import { CATALOGS, setLanguage } from '../i18n';
import type { MemberRoutes } from '../api/types';
import { ApiFailure } from '../api/transport';
import { LATENCY_CAP_MS, latencyNumber, latencyTone } from './latency';

const memberRoutes = vi.fn<(device: string) => Promise<MemberRoutes>>();
const setMemberRoute = vi.fn<(device: string, selection: unknown) => Promise<boolean>>();
vi.mock('../api/client', async original => {
  const actual = await original<typeof import('../api/client')>();
  return { ...actual, api: { ...actual.api, memberRoutes: (d: string) => memberRoutes(d), setMemberRoute: (d: string, s: unknown) => setMemberRoute(d, s) } };
});
const { compareAddress, formatRouteLatency, orderRows, routeItems, routeSummary } = await import('./RouteMenu');
const { SideContextMenu } = await import('./DeviceMenu');
import type { Device } from './types';

beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
beforeEach(() => { setLanguage('en'); memberRoutes.mockReset(); setMemberRoute.mockReset(); });
afterEach(() => { cleanup(); toast.dismiss(); });

const base: MemberRoutes = {
  device: 'remote', available: true, relayed: false, relay_via_name: null, passive: false, peer_ip: null, current_rtt_ms: 0.4,
  selection: { type: 'auto' }, remembered_ip: null, selection_unavailable: false, auto_choice: '10.77.0.2',
  pending_switch: null, can_replace: true, pending: false,
  rows: [
    { ip: '192.168.56.1', port: 47470, state: 'unreachable', rtt_ms: null, current: false },
    { ip: '192.168.1.20', port: 47470, state: 'reachable', rtt_ms: 13.4, current: false },
    { ip: '10.77.0.2', port: 47470, state: 'reachable', rtt_ms: 0.4, current: true },
    { ip: '100.101.102.103', port: 47470, state: 'measuring', rtt_ms: null, current: false },
    { ip: '172.17.0.1', port: 47470, state: 'unreachable', rtt_ms: null, current: false },
  ],
};
const passive: MemberRoutes = { ...base, passive: true, peer_ip: '10.77.0.1', rows: base.rows.map(r => ({ ...r, current: false })) };
const relayed: MemberRoutes = {
  ...base, relayed: true, relay_via_name: 'Hub', current_rtt_ms: 42.6,
  rows: [
    { ip: '10.77.0.2', port: 47470, state: 'measuring', rtt_ms: null, current: false },
    { ip: '192.168.1.20', port: 47470, state: 'unreachable', rtt_ms: null, current: false },
  ],
};

it('orders addresses by octets with unreachable ones last', () => {
  expect(['10.0.0.10', '10.0.0.9', '192.168.1.100', '192.168.1.20', '169.254.3.4'].sort(compareAddress))
    .toEqual(['10.0.0.9', '10.0.0.10', '169.254.3.4', '192.168.1.20', '192.168.1.100']);
  expect(orderRows(base.rows).map(r => r.ip)).toEqual(['10.77.0.2', '100.101.102.103', '192.168.1.20', '172.17.0.1', '192.168.56.1']);
});

it('shows whole milliseconds up to 999 in the device-list tiers; no measurement is grey', () => {
  const at = (rtt_ms: number | null, state: 'reachable' | 'unreachable' | 'measuring' = 'reachable') => formatRouteLatency({ state, rtt_ms });
  expect(at(0.4)).toEqual({ text: '0 ms', tone: 'good' });
  expect(at(0.5)).toEqual({ text: '1 ms', tone: 'good' });
  expect(at(13.4)).toEqual({ text: '13 ms', tone: 'good' });
  expect(at(19.99)).toEqual({ text: '20 ms', tone: 'good' });
  expect(at(20)).toEqual({ text: '20 ms', tone: 'fair' });
  expect(at(50)).toEqual({ text: '50 ms', tone: 'fair' });
  expect(at(50.01)).toEqual({ text: '50 ms', tone: 'poor' });
  expect(at(998.6)).toEqual({ text: '999 ms', tone: 'poor' });
  expect(at(4321)).toEqual({ text: '999 ms', tone: 'poor' });
  expect(at(null, 'measuring')).toEqual({ text: 'Measuring…', tone: 'none' });
  expect(at(null, 'unreachable')).toEqual({ text: 'No response', tone: 'none' });
  expect(at(3, 'unreachable').tone).toBe('none');
  // One formatter and one tier table with the device list.
  expect(LATENCY_CAP_MS).toBe(999);
  expect(latencyNumber(1234.5)).toBe(999);
  expect(latencyNumber(2.5)).toBe(3);
  expect([latencyTone(19.99), latencyTone(20), latencyTone(50.01), latencyTone(null)]).toEqual(['good', 'fair', 'poor', undefined]);
  const css = readFileSync('src/devices/devices.css', 'utf8');
  expect(css).toMatch(/\.devmenu__route-ms\[data-tone="good"\] \{ color: var\(--ok-success\); \}/);
  expect(css).toMatch(/\.devmenu__route-ms\[data-tone="fair"\] \{ color: var\(--ok-warning\); \}/);
  expect(css).toMatch(/\.devmenu__route-ms\[data-tone="poor"\] \{ color: var\(--ok-danger\); \}/);
  expect(css).toMatch(/\.devmenu__route-ms\[data-tone="none"\] \{ color: var\(--ok-text-3\); \}/);
});

it('active end: no incoming row; selected automatic shows only the peer address in use and its latency', () => {
  const items = routeItems(base, 'Win-PC');
  expect(items.value).toBe('auto');
  expect(items.top).toBeNull();
  expect(items.auto).toEqual({ disabled: false, hint: '10.77.0.2', tip: null, latency: { text: '0 ms', tone: 'good' } });
  expect(items.rows.find(r => r.ip === '172.17.0.1')).toMatchObject({ disabled: true, latency: { text: 'No response', tone: 'none' } });
  expect(items.rows.find(r => r.ip === '10.77.0.2')).toMatchObject({ disabled: false, latency: { text: '0 ms', tone: 'good' } });
  // Not even a pending switch adds words under "Automatic".
  expect(routeItems({ ...base, pending_switch: '192.168.1.20' }, 'Win-PC').auto.hint).toBe('10.77.0.2');
});

it('automatic that is not selected shows neither a hint nor a latency', () => {
  const pinned = routeItems({ ...base, selection: { type: 'address', ip: '192.168.1.20' } }, 'Win-PC');
  expect(pinned.value).toBe('ip:192.168.1.20');
  expect(pinned.auto).toMatchObject({ hint: null, latency: null });
  expect(pinned.rows.find(r => r.ip === '192.168.1.20')?.latency.text).toBe('13 ms');
  // An unavailable remembered target is disabled while automatic is selected.
  const gone = routeItems({ ...base, selection: { type: 'auto' }, remembered_ip: '172.17.0.1', selection_unavailable: true }, 'Win-PC');
  expect(gone.value).toBe('auto');
  expect(gone.rows.find(r => r.ip === '172.17.0.1')?.disabled).toBe(true);
  expect(gone.auto.hint).toBe('10.77.0.2');
});

it('accepting end selects automatic with the peer name and actual peer IP, even without own routes', () => {
  const auto = routeItems(passive, 'Win-PC');
  expect(auto.top).toBeNull();
  expect(auto.value).toBe('auto');
  expect(auto.auto).toMatchObject({ hint: 'From Win-PC (10.77.0.1)', latency: { text: '0 ms', tone: 'good' } });
  const stuck = routeItems({ ...passive, can_replace: false }, 'Win-PC');
  expect(stuck.auto).toMatchObject({ disabled: false, tip: null });
  expect(stuck.rows.every(r => r.disabled && r.tip === "This device can't reach Win-PC")).toBe(true);
});

it('relayed: a selected relay row "via <device>" with the end-to-end latency; untried addresses stay choosable', () => {
  const items = routeItems(relayed, 'Win-PC');
  expect(items.value).toBe('relay');
  expect(items.top).toEqual({ kind: 'relay', hint: 'via Hub', latency: { text: '43 ms', tone: 'fair' } });
  expect(items.auto).toEqual({ disabled: false, hint: null, tip: null, latency: null });
  expect(items.rows).toEqual([
    { ip: '10.77.0.2', disabled: false, latency: { text: 'Measuring…', tone: 'none' }, tip: null },
    { ip: '192.168.1.20', disabled: true, latency: { text: 'No response', tone: 'none' }, tip: null },
  ]);
  // A relay with no measured RTT says so in grey instead of a number.
  expect(routeItems({ ...relayed, current_rtt_ms: null }, 'Win-PC').top?.latency).toEqual({ text: 'Measuring…', tone: 'none' });
  // Relayed and passive never show together.
  expect(routeItems({ ...relayed, passive: true }, 'Win-PC').top?.kind).toBe('relay');
});

it('route strings exist in all four languages and the dropped ones are gone', () => {
  for (const key of ['选择线路', '中转', '经 {0}', '自动', '{0}（{1}） 连入', '无法从本机连接到 {0}', '测量中…', '无响应'] as const) {
    for (const [language, catalog] of Object.entries(CATALOGS)) expect((catalog as Record<string, unknown>)[key], `${language} ${key}`).toBeTruthy();
  }
  for (const key of ['被动', '{0} 连入 · {1}', '{0} 连入', '由 {0} 指定', '{0} 指定的线路暂时不可用', '只有 {0} 连到本机时才能选择', '使用中：{0}', '使用中：{0} 连入',
    '使用中：{0}，空闲后切换到 {1}', '将使用：{0}', 'route|当前', '没有可用的直连线路']) {
    for (const [language, catalog] of Object.entries(CATALOGS)) expect((catalog as Record<string, unknown>)[key], `${language} ${key}`).toBeUndefined();
  }
  setLanguage('zh-Hans');
  expect(routeItems(passive, '手机').auto.hint).toBe('手机（10.77.0.1） 连入');
  expect(CATALOGS.en['经 {0}']).toBe('via {0}');
});

it('card tooltip names the direct route only', () => {
  expect(routeSummary(base, 'Win-PC')).toBe('Direct · 10.77.0.2 · 0 ms');
  expect(routeSummary(passive, 'Win-PC')).toBe('Direct · From Win-PC (10.77.0.1) · 0 ms');
  expect(routeSummary(relayed, 'Win-PC')).toBeNull();
  expect(routeSummary(undefined, 'Win-PC')).toBeNull();
});

const remote: Device = {
  id: 'remote', name: 'Remote', os: 'win', group: 'online', ip: '', port: 0, latency: 3, lastSeen: null,
  cursor: 'remote', screen: 'shared', screenSharing: true, mouse: 'shared', keyboard: 'shared', clipboard: 'shared', canDisableMouse: true,
  speaker: { mode: 'unused', volume: 50, volumeKnown: true, muted: false, canMakeExit: true }, gameMode: 'auto',
};
const phone: Device = {
  ...remote, id: 'phone', name: 'Phone', os: 'android', screen: 'unknown', screenSharing: undefined, mouse: 'unknown', keyboard: 'unknown', gameMode: null,
};
function openMenu(d: Device = remote) {
  const noop = () => {};
  render(<OinkProvider defaultMotion="reduced"><Toaster /><SideContextMenu
    devices={[d]} requests={[]} nearby={[]} onShare={noop} onScreensHidden={noop} onHideScreen={noop}
    onGameMode={noop} onPause={noop} onLeave={noop} onEditAddress={noop}
    onShowWindow={noop} onAbout={noop} onQuit={noop} onSpeaker={noop}
    onAccept={noop} onReject={noop} onPair={noop} onBlock={noop}
  ><button data-ctx={`device:${d.id}`}>Target</button></SideContextMenu></OinkProvider>);
  fireEvent.contextMenu(screen.getByRole('button', { name: 'Target' }));
}
async function openRoutes(d: Device = remote) {
  openMenu(d);
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Connection Route' }));
  await waitFor(() => expect(memberRoutes).toHaveBeenCalledWith(d.id));
  return screen.findAllByRole('menuitemradio');
}
/** Top-level menu entries with separators as '|'. */
async function menuShape() {
  const menu = await screen.findByRole('menu');
  return [...menu.querySelectorAll('[role^="menuitem"], [role="separator"]')]
    .filter(e => e.closest('[role="menu"]') === menu)
    .map(e => e.getAttribute('role') === 'separator' ? '|' : e.textContent);
}

it('the route submenu sits in the same place for PCs and phones: its own section right before the last one', async () => {
  openMenu();
  const pc = await menuShape();
  expect(pc.slice(pc.indexOf('Connection Route') - 1)).toEqual(['|', 'Connection Route', '|', 'Pause sharing', 'Remove from group']);
  cleanup();
  openMenu(phone);
  expect(await menuShape()).toEqual(['Connection Route', '|', 'Remove from group']);
  cleanup();
  openMenu({ ...phone, group: 'offline' });
  expect(await menuShape()).not.toContain('Connection Route');
});

it('a phone uses the same route rules: a direct-only phone has no relay row', async () => {
  memberRoutes.mockResolvedValue({ ...base, device: 'phone' });
  setMemberRoute.mockResolvedValue(true);
  const radios = await openRoutes(phone);
  expect(radios.map(r => r.textContent)).not.toContain('Relay');
  expect(radios[0].textContent).toContain('Automatic');
  expect(radios[0].querySelector('.devmenu__route-hint')?.textContent).toBe('10.77.0.2');
  expect(radios.slice(1).map(r => r.querySelector('.devmenu__route-ip')?.textContent)).toEqual(['10.77.0.2', '100.101.102.103', '192.168.1.20', '172.17.0.1', '192.168.56.1']);
  await act(async () => { fireEvent.click(radios[3]); });
  expect(setMemberRoute).toHaveBeenCalledWith('phone', { type: 'address', ip: '192.168.1.20' });
});

it('a phone that dialed this PC shows automatic and its IP; relay presentation stays the same', async () => {
  memberRoutes.mockResolvedValue({ ...passive, device: 'phone' });
  let radios = await openRoutes(phone);
  expect(radios[0].textContent).toContain('Automatic');
  expect(radios[0].textContent).toContain('From Phone (10.77.0.1)');
  expect(radios[0].getAttribute('aria-checked')).toBe('true');
  expect(radios.some(r => r.textContent === 'Incoming')).toBe(false);
  cleanup();
  memberRoutes.mockReset();
  memberRoutes.mockResolvedValue({ ...relayed, device: 'phone' });
  radios = await openRoutes(phone);
  expect(radios[0].textContent).toContain('via Hub');
});

it('opening the submenu reads the stored result and applies a choice at once', async () => {
  memberRoutes.mockResolvedValue({ ...base, device: 'remote' });
  setMemberRoute.mockResolvedValue(true);
  const radios = await openRoutes();
  expect(radios[0].textContent).toContain('Automatic');
  expect(radios[0].querySelector('.devmenu__route-hint')?.textContent).toBe('10.77.0.2');
  expect(radios.slice(1).map(r => r.querySelector('.devmenu__route-ip')?.textContent)).toEqual(['10.77.0.2', '100.101.102.103', '192.168.1.20', '172.17.0.1', '192.168.56.1']);
  expect(screen.queryByText('Incoming')).toBeNull();
  expect(screen.queryByText('In use')).toBeNull();
  // No separator inside the route list, and every row has the same vertical inset.
  expect(radios[0].closest('[role="group"]')?.querySelector('[role="separator"]')).toBeNull();
  expect(radios.every(r => r.classList.contains('devmenu__route-item'))).toBe(true);
  expect(radios[3].querySelector('.devmenu__route-ms')?.getAttribute('data-tone')).toBe('good');
  expect(radios[4].querySelector('.devmenu__route-ms')?.getAttribute('data-tone')).toBe('none');
  await act(async () => { fireEvent.click(radios[3]); });
  expect(setMemberRoute).toHaveBeenCalledWith('remote', { type: 'address', ip: '192.168.1.20' });
});

it('a relayed link lists the relay first and lets an untried address be chosen', async () => {
  memberRoutes.mockResolvedValue({ ...relayed, device: 'remote' });
  setMemberRoute.mockResolvedValue(true);
  const radios = await openRoutes();
  expect(radios[0].textContent).toContain('Relay');
  expect(radios[0].textContent).toContain('via Hub');
  expect(radios[0].getAttribute('aria-checked')).toBe('true');
  expect(radios[1].textContent).toBe('Automatic');
  expect(radios[3].getAttribute('aria-disabled')).toBe('true');
  await act(async () => { fireEvent.click(radios[2]); });
  expect(setMemberRoute).toHaveBeenCalledWith('remote', { type: 'address', ip: '10.77.0.2' });
});

it('a refused choice says so and the menu keeps the real route', async () => {
  memberRoutes.mockResolvedValue({ ...base, device: 'remote' });
  setMemberRoute.mockResolvedValue(false);
  const radios = await openRoutes();
  await act(async () => { fireEvent.click(radios[3]); });
  expect(setMemberRoute).toHaveBeenCalledWith('remote', { type: 'address', ip: '192.168.1.20' });
  const notice = await screen.findByText("Couldn't switch the connection route");
  expect(notice.closest('.ok-toast')?.textContent).toContain("This route isn't available right now");
  // The stored view is read again, so reopening shows what is really in use.
  await waitFor(() => expect(memberRoutes).toHaveBeenCalledTimes(2));
});

it('a failed choice reports the problem', async () => {
  memberRoutes.mockResolvedValue({ ...base, device: 'remote' });
  setMemberRoute.mockRejectedValue(new ApiFailure({ code: 'Unavailable', problem: { code: 'link_lost' } } as never));
  const radios = await openRoutes();
  await act(async () => { fireEvent.click(radios[3]); });
  const notice = await screen.findByText("Couldn't switch the connection route");
  expect(notice.closest('.ok-toast')?.textContent).toContain(CATALOGS.en['连接已断开']);
  await waitFor(() => expect(memberRoutes).toHaveBeenCalledTimes(2));
});

it('route failure strings exist in every language', () => {
  for (const key of ['无法切换线路', '这条线路现在不可用'] as const) {
    for (const [language, catalog] of Object.entries(CATALOGS)) expect((catalog as Record<string, unknown>)[key], `${language} ${key}`).toBeTruthy();
  }
});

it('every route row keeps the single-line item inset, hint or not', () => {
  const css = readFileSync('src/devices/devices.css', 'utf8');
  expect(css).toMatch(/\.devmenu__route-item \{ padding-block: calc\(\(var\(--ok-h-item\) - 1\.4em\) \/ 2\); \}/);
});

it('clicking selected automatic still clears an unavailable remembered choice', async () => {
  memberRoutes.mockResolvedValue({ ...passive, device: 'remote', remembered_ip: '172.17.0.1', selection_unavailable: true, can_replace: false });
  setMemberRoute.mockResolvedValue(true);
  const radios = await openRoutes();
  expect(radios[0].getAttribute('aria-checked')).toBe('true');
  expect(radios[0].getAttribute('aria-disabled')).not.toBe('true');
  await act(async () => { fireEvent.click(radios[0]); });
  expect(setMemberRoute).toHaveBeenCalledWith('remote', { type: 'auto' });
});

it('keeps a withdrawn remembered IP visible and unavailable until replaced', () => {
  const items = routeItems({ ...base, remembered_ip: '203.0.113.9', selection_unavailable: true }, 'Remote');
  expect(items.value).toBe('auto');
  expect(items.rows.find(row => row.ip === '203.0.113.9')).toMatchObject({ disabled: true, latency: { text: 'No response', tone: 'none' } });
});

it('an authenticated remembered IP stays grey until the switch actually finishes', () => {
  const items = routeItems({ ...base, remembered_ip: '192.168.1.20', selection_unavailable: true }, 'Remote');
  expect(items.value).toBe('auto');
  expect(items.rows.find(row => row.ip === '192.168.1.20')?.disabled).toBe(true);
});
