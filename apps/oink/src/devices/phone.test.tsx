// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { OinkProvider } from 'oink-ui';
import { setLanguage } from '../i18n';
import { SideContextMenu } from './DeviceMenu';
import type { SideContextMenuProps } from './DeviceMenu';
import { DeviceItem, audioTooltip, indicators } from './DeviceList';
import { presentMobileAudio } from './project';
import { mobileAudioStart, startOutcome } from './mobileAudio';
import type { Device } from './types';
import type { MobileAudioState, MobileEndpointState } from '../api/types';

vi.mock('../api/client', () => ({
  api: { startMobileAudioEndpoint: vi.fn(async () => null) },
}));
import { api } from '../api/client';

beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
});
afterEach(() => { cleanup(); setLanguage('en'); });

const phone = (os: 'android' | 'ios' = 'android', extra: Partial<Device> = {}): Device => ({
  id: 'phone', name: 'Phone', os, group: 'online', ip: '', port: 0, latency: 3, lastSeen: null,
  cursor: 'remote', screen: 'unknown', mouse: 'unknown', keyboard: 'unknown', clipboard: 'shared',
  speaker: { mode: 'unused', volume: 40, volumeKnown: true, muted: false, canMakeExit: true, canSetVolume: true },
  microphone: { mode: 'unavailable', permissionGranted: true, virtualReady: false, volume: 100, muted: false, canSetVolume: false },
  gameMode: null, ...extra,
});
/** `running`: the endpoint's phase (Running or Off); it never says the mode. */
const endpoint = (running: boolean, start_allowed: boolean, direction: 'microphone_send' | 'speaker_receive' | 'speaker_send'): MobileEndpointState => ({
  supported: true, start_allowed, phase: running ? 2 : 0, reason: 0,
  target: { boot: { device_id: 'phone', nonce: [1] }, direction, generation: '7', activation_id: null },
});
const state = (speaker: [boolean, boolean], microphone: [boolean, boolean]): MobileAudioState => ({
  revision: '3', owner: null,
  speaker_receive: endpoint(...speaker, 'speaker_receive'),
  speaker_send: endpoint(false, false, 'speaker_send'),
  microphone_send: endpoint(...microphone, 'microphone_send'),
});
/** The card mode (global audio configuration) of a phone: exit and/or sender. */
const roles = (exit: boolean, sender: boolean, extra: Partial<Device> = {}): Device => {
  const d = phone(extra.os === 'ios' ? 'ios' : 'android', extra);
  return {
    ...d,
    speaker: exit ? { ...d.speaker, mode: 'exit', configuredMode: 'receiver' } : { ...d.speaker, mode: 'unused', configuredMode: 'unused' },
    microphone: d.microphone && { ...d.microphone, mode: sender ? 'source' : 'unavailable' },
  };
};
const present = (d: Device, s: MobileAudioState) => presentMobileAudio([d], new Map([[d.id, s]]))[0];

describe('phone audio modes come from the global audio configuration', () => {
  it('shows the card mode and never desktop forwarding/receive gestures', () => {
    const d = present(roles(true, false), state([true, false], [false, false]));
    expect(d.speaker.mode).toBe('exit');
    expect(indicators(d).map(i => i.glyph)).toEqual(['speaker', 'microphone']);
    expect(audioTooltip(d, 'speaker', null)).toEqual(['Speaker: Audio exit', 'Click: Mute', 'Double-click: Switch back to local']);
    // A normal phone cannot be started from the PC.
    expect(audioTooltip(d, 'microphone', null)).toEqual(['Microphone: Local']);
    const privileged = present(roles(false, false), state([false, true], [false, true]));
    expect(audioTooltip(privileged, 'speaker', null)).toEqual(['Speaker: Local', 'Double-click: Use phone speaker']);
    expect(audioTooltip(privileged, 'microphone', null)).toEqual(['Microphone: Local', 'Double-click: Use phone microphone']);
  });

  it('draws a receiving speaker and a sending microphone with the icons of a PC in that state', () => {
    const icon = ({ glyph, tone, badge }: ReturnType<typeof indicators>[number]) => ({ glyph, tone, badge });
    const on = indicators(present(roles(true, true), state([true, false], [true, false]))).map(icon);
    const pc: Device = { ...phone(), os: 'win',
      speaker: { ...phone().speaker, mode: 'exit' },
      microphone: { ...phone().microphone!, mode: 'source' } };
    const [, , pcSpeaker, pcMicrophone] = indicators(pc).map(icon);
    expect(on).toEqual([pcSpeaker, pcMicrophone]);
    expect(on).toEqual([{ glyph: 'speaker', tone: 'accent', badge: 'receive' }, { glyph: 'microphone', tone: 'accent', badge: 'send' }]);
  });

  it('an endpoint running alone never makes the phone the exit or the sender (F1/F2)', () => {
    // The phone lost the exit to a PC; its endpoint has not stopped yet.
    const d = present(roles(false, false), state([true, false], [true, false]));
    expect(d.speaker.mode).toBe('unused');
    expect(d.microphone?.mode).toBe('unavailable');
    expect(d.mobileAudio).toEqual({ speaker: { enabled: false, canStart: false }, microphone: { enabled: false, canStart: false }, forward: { enabled: false, canStart: false } });
    // The configuration says exit before the endpoint started: exit.
    const starting = present(roles(true, true), state([false, false], [false, false]));
    expect(starting.speaker.mode).toBe('exit');
    expect(starting.mobileAudio?.speaker.enabled).toBe(true);
    expect(starting.mobileAudio?.microphone.enabled).toBe(true);
  });

  it('shows an interrupted or refused endpoint as a fault with its reason, not as active (M23)', () => {
    const s = state([true, false], [false, true]);
    s.speaker_receive = { ...s.speaker_receive, phase: 3, reason: 4 };
    s.microphone_send = { ...s.microphone_send, refusal: 3 };
    const d = present(roles(true, false), s);
    expect(d.speaker.mode).toBe('fault');
    expect(audioTooltip(d, 'speaker', null)[0]).toBe('Speaker: Unavailable (Device unavailable)');
    expect(audioTooltip(d, 'microphone', null)[0]).toBe('Microphone: Unavailable (An operation is in progress)');
    const denied = state([false, false], [true, false]);
    denied.microphone_send = { ...denied.microphone_send, phase: 4, reason: 6 };
    expect(audioTooltip(present(roles(false, true), denied), 'microphone', null)[0]).toBe('Microphone: Unavailable (System permission not granted)');
  });

  it('double-click turns an exit back to local and starts only when allowed', async () => {
    const s = state([true, false], [false, true]);
    expect(mobileAudioStart(s, 'speaker')).toBeNull();
    await mobileAudioStart(s, 'microphone')!();
    expect(api.startMobileAudioEndpoint).toHaveBeenCalledWith(s.microphone_send.target);

    // "切回本地" is the card's audio mode (Clear(phone)), handed to the app.
    const onMobileAudio = vi.fn();
    render(<OinkProvider defaultMotion="reduced"><DeviceItem d={present(roles(true, false), s)} now={0} selected={false} collapsed={false} onSelect={() => {}} onMobileAudio={onMobileAudio} onSpeaker={() => {}} onMicrophone={() => {}}/></OinkProvider>);
    const speaker = screen.getAllByRole('button').find(b => b.getAttribute('data-glyph') === 'speaker')!;
    fireEvent.click(speaker, { detail: 1 });
    fireEvent.click(speaker, { detail: 2 });
    expect(onMobileAudio).toHaveBeenCalledWith('speaker', 'close');
  });
});

describe('phone endpoint faults', () => {
  const withPhase = (s: MobileAudioState, kind: 'speaker_receive' | 'microphone_send', phase: number, reason: number): MobileAudioState =>
    ({ ...s, [kind]: { ...s[kind], phase, reason } });

  it('shows an interrupted or failed endpoint as a fault with the phone reason, not as exit or sending', () => {
    // Speaker enabled but interrupted (phase 3, reason Interrupted=4); the
    // microphone failed for missing permission (phase 4, reason 6).
    const s = withPhase(withPhase(state([true, false], [true, false]), 'speaker_receive', 3, 4), 'microphone_send', 4, 6);
    const d = present(roles(true, true), s);
    expect(d.speaker.mode).toBe('fault');
    expect(d.faults?.speaker).toBe('Device unavailable');
    expect(audioTooltip(d, 'speaker', null)[0]).toBe('Speaker: Unavailable (Device unavailable)');
    expect(d.microphone?.fault?.code).toBe('permission_required');
    expect(indicators(d).map(i => i.badge)).toEqual(['warning', 'warning']);
  });

  it('reports how a PC-initiated start settled', () => {
    const pending = state([false, true], [false, true]);
    expect(startOutcome(pending, 'microphone')).toBeNull();
    expect(startOutcome(withPhase(state([false, true], [false, false]), 'microphone_send', 1, 0), 'microphone')).toBeNull();
    expect(startOutcome(state([false, true], [true, false]), 'microphone')).toBe('started');
    expect(startOutcome(withPhase(pending, 'microphone_send', 4, 6), 'microphone')).toBe('permission_required');
    // Pause audit 2: the phone answers a start it did not carry out, so the
    // PC's pending start ends with the reason instead of hanging.
    for (const [refusal, code] of [[2, 'helper_unavailable'], [3, 'busy'], [6, 'request_expired'], [7, 'request_declined']] as const) {
      const answered = state([false, true], [false, true]);
      answered.microphone_send = { ...answered.microphone_send, refusal };
      expect(startOutcome(answered, 'microphone')).toBe(code);
    }
  });
});

describe('phone context menu', () => {
  function menu(d: Device, props: Partial<SideContextMenuProps> = {}) {
    const noop = () => {};
    render(<OinkProvider defaultMotion="reduced"><SideContextMenu
      devices={[d]} requests={[]} nearby={[]} onShare={noop} onScreensHidden={noop} onHideScreen={noop}
      onGameMode={noop} onPause={noop} onLeave={noop} onEditAddress={noop}
      onShowWindow={noop} onAbout={noop} onQuit={noop} onSpeaker={noop}
      onAccept={noop} onReject={noop} onPair={noop} onBlock={noop} {...props}
    ><button data-ctx={`device:${d.id}`}>Target</button></SideContextMenu></OinkProvider>);
    fireEvent.contextMenu(screen.getByRole('button', { name: 'Target' }));
  }
  const items = async () => [...(await screen.findByRole('menu')).querySelectorAll('[role^="menuitem"]')].map(i => i.textContent);

  it.each(['online', 'offline'] as const)('restores a hidden phone screen through the existing device menu while %s', async group => {
    setLanguage('en');
    const onScreensHidden = vi.fn();
    menu(phone('android', { group, privileged: true }), {
      screens: [{id:'phone:android:built-in',device:'phone',name:'Built-in display',hidden:true}],
      onScreensHidden,
    });
    fireEvent.click(await screen.findByRole('menuitem', { name: 'Show all screens' }));
    expect(onScreensHidden).toHaveBeenCalledWith('phone', false);
  });

  it('offers no screen, input, game or pause items and only endpoint audio actions', async () => {
    const onMobileAudio = vi.fn();
    menu(present(roles(true, false), state([true, false], [false, true])), { onMobileAudio });
    expect(await items()).toEqual(['Stop using phone speaker', 'Use phone microphone', 'Connection Route', 'Remove from group']);
    fireEvent.click(screen.getByRole('menuitem', { name: 'Stop using phone speaker' }));
    expect(onMobileAudio).toHaveBeenCalledWith('phone', 'speaker', 'close');
  });
  it('offers source start and stop only on Android', async () => {
    const s = state([false, true], [false, true]);
    s.speaker_send = endpoint(false, true, 'speaker_send');
    const onMobileAudio = vi.fn();
    menu(present(roles(false, false), s), { onMobileAudio });
    await items();
    fireEvent.click(screen.getByRole('menuitem', { name: 'Request phone audio forwarding' }));
    expect(onMobileAudio).toHaveBeenCalledWith('phone', 'forward', 'start');
    cleanup();
    menu(present(roles(false, false, {os: 'ios'}), s));
    expect(await items()).not.toContain('Request phone audio forwarding');
    cleanup();
    const d = phone('android', { speaker: { ...phone().speaker, mode: 'forwarding', configuredMode: 'source' } });
    menu(present(d, s), { onMobileAudio });
    await items();
    fireEvent.click(screen.getByRole('menuitem', { name: 'Stop phone audio forwarding' }));
    expect(onMobileAudio).toHaveBeenCalledWith('phone', 'forward', 'close');
  });
  it('offers nothing to start on a normal phone', async () => {
    menu(present(roles(false, false, { os: 'ios' }), state([false, false], [false, false])));
    expect(await items()).toEqual(['Connection Route', 'Remove from group']);
  });
  it('offline phone has no screen visibility item', async () => {
    menu(phone('ios', { group: 'offline' }));
    expect(await items()).toEqual(['Edit IP', 'Remove from group']);
  });
});

describe('phone badges', () => {
  const item = (d: Device, collapsed = false) => render(<OinkProvider defaultMotion="reduced"><DeviceItem d={d} now={0} selected={false} collapsed={collapsed} onSelect={() => {}}/></OinkProvider>).container;
  it('draws the Android robot head and the Apple logo with an iPhone or iPad corner', () => {
    expect(item(phone()).querySelector('svg.os-logo[data-os="android"] .os-logo__knock')).toBeTruthy();
    cleanup();
    expect(item(phone('ios', { formFactor: 'phone' })).querySelector('.os-apple[data-device="iphone"] .os-apple__corner')).toBeTruthy();
    cleanup();
    expect(item(phone('ios', { formFactor: 'tablet' })).querySelector('.os-apple[data-device="ipad"]')).toBeTruthy();
  });
  it('crowns the Android head only in privileged mode and announces it', () => {
    const plain = item(phone());
    const head = plain.querySelector('svg.os-logo[data-os="android"]')!;
    expect(head.hasAttribute('data-privileged')).toBe(false);
    expect(head.querySelector('.os-logo__crown, .os-logo__crown-edge')).toBeNull();
    cleanup();
    const crowned = item(phone('android', { privileged: true }));
    const badge = crowned.querySelector('svg.os-logo[data-os="android"][data-privileged]')!;
    expect(badge.getAttribute('role')).toBe('img');
    expect(badge.getAttribute('aria-label')).toBe('Android, privileged mode on');
    expect(screen.getByRole('img', { name: 'Android, privileged mode on' })).toBe(badge);
    // Head unchanged (eyes still knocked out); the crown is the edge layer under the fill layer.
    expect(badge.querySelector('.os-logo__knock')).toBeTruthy();
    const layers = [...badge.querySelectorAll('path')].map(p => p.getAttribute('class'));
    expect(layers.slice(2)).toEqual(['os-logo__crown-edge', 'os-logo__crown']);
    expect(badge.querySelector('.os-logo__crown-edge')!.getAttribute('stroke-width')).toBe('5');
    cleanup();
    setLanguage('zh-Hans');
    expect(item(phone('android', { privileged: true })).querySelector('[data-privileged]')!.getAttribute('aria-label')).toBe('Android，特权模式已开启');
  });
  it('colour tokens: file green in both themes, gold crown with a light-only darker rim', () => {
    const css = readFileSync('src/devices/devices.css', 'utf8');
    expect(css).toContain(':root { --c-os-android: #34A853; --c-crown: #F0B400; --c-crown-edge: #A87500; }');
    expect(css).toContain(':root[data-theme="dark"] { --c-crown: #F5B400; --c-crown-edge: #F5B400; }');
    expect(css).toMatch(/\.os-logo\[data-os="android"\] \{[^}]*color: var\(--c-os-android\)/);
    expect(css).toMatch(/\.os-logo__crown-edge \{ fill: var\(--c-crown-edge\); stroke: var\(--c-crown-edge\); \}/);
    expect(css).toMatch(/\.os-logo__crown \{ fill: var\(--c-crown\); \}/);
    expect(css).not.toContain('#3DDC84');
  });
  it('the crowned badge keeps the head where the plain badge draws it', () => {
    const box = (viewBox: string) => viewBox.split(' ').map(Number);
    const [, headY, headW, headH] = box(item(phone()).querySelector('svg.os-logo')!.getAttribute('viewBox')!);
    cleanup();
    const [, crownY, crownW, crownH] = box(item(phone('android', { privileged: true })).querySelector('svg.os-logo')!.getAttribute('viewBox')!);
    expect(crownW).toBe(headW);
    // Equal padding above and below keeps the head centred at the same spot.
    expect(headY - crownY).toBeCloseTo((crownY + crownH) - (headY + headH));
  });
  it('collapsed: the upgrade mark replaces the offline signal in the same corner', () => {
    const c = item(phone('android', { group: 'offline', offlineReason: 'protocol_incompatible' }), true);
    expect(c.querySelector('.os-upgrade')).toBeTruthy();
    expect(c.querySelector('.os-signal')?.hasAttribute('data-shown')).toBe(false);
  });
});


describe('phone audio forwarding', () => {
  it('uses the PC gain slider and forwarding tooltip', async () => {
    const s = state([false, false], [false, true]);
    s.speaker_send = endpoint(true, false, 'speaker_send');
    const d = present(phone('android', { speaker: { ...phone().speaker, mode: 'forwarding', configuredMode: 'source', volume: 170 } }), s);
    expect(d.mobileAudio?.forward?.enabled).toBe(true);
    expect(audioTooltip(d, 'speaker', 'Exit')[0]).toBe('Speaker: Forwarding to Exit');
    render(<OinkProvider><DeviceItem d={d} now={0} selected collapsed={false} onSelect={() => {}} onSpeaker={() => {}}/></OinkProvider>);
    expect((await screen.findByRole('slider')).getAttribute('aria-valuemax')).toBe('200');
  });
  it('starts the source direction, independent of speaker receive', async () => {
    const s = state([false, true], [false, true]);
    s.speaker_send = endpoint(false, true, 'speaker_send');
    await mobileAudioStart(s, 'forward')?.();
    expect(api.startMobileAudioEndpoint).toHaveBeenLastCalledWith(s.speaker_send.target);
    s.speaker_send = endpoint(true, false, 'speaker_send');
    expect(startOutcome(s, 'forward')).toBe('started');
  });
});
