// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { toast } from 'oink-ui';
import { App } from './App';
import { agent } from './api/store';
import type { DeviceCardsView } from './api/cards';
import type { ArrangementSnapshot, DeviceArrangementState, PreviewReply } from './api/layoutProfiles';
import { layoutProfilesFixtureSnapshot } from './api/layoutProfiles.fixture';
import type { Placement } from './api/types';
import { cameraStore } from './arrange/cameraStore';
import { profileHarness } from './arrange/profileHarness';
import { setLanguage } from './i18n';

const backend = vi.hoisted(() => ({ harness: null as null | ReturnType<typeof import('./arrange/profileHarness').profileHarness> }));
vi.mock('./api/store', async original => ({ ...await original<typeof import('./api/store')>(), start: vi.fn() }));
vi.mock('./api/transport', async original => ({
  ...await original<typeof import('./api/transport')>(),
  transport: {
    call: (request: { method: string; params?: Record<string, unknown> }) => backend.harness!.call(request),
    shell: async () => null,
    onEvent: async () => () => {},
  },
}));

const [A, B, C] = ['a', 'b', 'c'].map(d => d.repeat(64));
const NAMES: Record<string, string> = { [A]: 'Alpha', [B]: 'Beta', [C]: 'Gamma' };
const cards = { schema: 1, stream: 'test', revision: '1', group: null, generatedAtMs: 0, exitVolume: null, emergencyDelivery: null,
  cards: [A, B, C].map(device => ({ device, name: NAMES[device], os: 'macos', logo: 'macos', isSelf: device === A,
    section: device === A ? 'this_device' : 'online', lastSeenMs: null, status: null, pause: null,
    connection: device === A ? { kind: 'self' } : { kind: 'online', signal: { kind: 'unknown', rttMs: null, bars: 0, colour: 'grey' } } })) } as DeviceCardsView;
/** Alpha writes its system arrangement when `sync` is on; Beta and Gamma stay rigid. */
const devices = (sync: boolean): DeviceArrangementState[] => [A, B, C].map(device => ({
  device, sync_system_arrangement: sync && device === A, preference_token: `preference:${device}:1`,
  native_token: `native:${device}:1`, availability: 'ready', can_edit_screens: sync && device === A,
}));
const clone = <T,>(v: T): T => structuredClone(v);

beforeAll(() => {
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  vi.stubGlobal('PointerEvent', MouseEvent);
  HTMLElement.prototype.setPointerCapture = () => {};
  HTMLElement.prototype.scrollTo = () => {};
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(() => ({ left: 0, top: 0, x: 0, y: 0, width: 900, height: 600, right: 900, bottom: 600, toJSON() {} }));
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
beforeEach(() => {
  setLanguage('en');
  agent.adopt('boot', { platform: 'macos', version: '0.1.0', build: null, backend: 'agent' });
  agent.adopt('cards', cards);
});
afterEach(() => { backend.harness?.stop(); toast.dismiss(); cleanup(); agent.reset(); localStorage.clear(); });

function setup(sync = false) {
  const h = backend.harness = profileHarness({ snapshot: layoutProfilesFixtureSnapshot(), devices: devices(sync) });
  agent.adopt('layout', h.fixture.layout());
  return h;
}
const button = (name: string) => screen.getByRole('button', { name }) as HTMLButtonElement;
const edit = (h: ReturnType<typeof setup>, change: (s: ArrangementSnapshot) => void) => {
  const next = clone(h.fixture.state().snapshot!); change(next);
  act(() => h.fixture.replaceCurrent(next));
};
const profilesOf = (h: ReturnType<typeof setup>) => h.fixture.state().profiles.map(p => p.name.kind === 'default' ? 'Default layout' : p.name.value);
const create = async (h: ReturnType<typeof setup>, name: string) => {
  await act(async () => { await h.fixture.api.createLayoutProfile({ request_id: `create-${name}`, expected_token: h.fixture.state().token, name }); });
};
// `hidden`: the canvas is inert while Settings is open.
const current = () => screen.getByRole('button', { name: /^Current layout:/, hidden: true }) as HTMLButtonElement;
async function openMenu() {
  fireEvent.click(current());
  return within(await screen.findByRole('menu')).getAllByRole('menuitemradio');
}
const LONG = 'A layout name that is 32 chars!!';

describe('current profile in the toolbar', () => {
  it('leads the toolbar with the current profile menu and +, before undo/redo and auto-compact', () => {
    setup();
    render(<App/>);
    const tools = screen.getByRole('toolbar', { name: 'Arrangement actions' });
    const group = within(tools).getByRole('group', { name: 'Layout' });
    expect(Array.from(group.querySelectorAll('button'), b => b.getAttribute('aria-label'))).toEqual(['Current layout: Default layout', 'New layout']);
    expect(Array.from(tools.querySelectorAll('button'), b => b.getAttribute('aria-label'))).toEqual(
      ['Current layout: Default layout', 'New layout', 'Undo', 'Redo', 'Auto-compact']);
    expect(current().className).toContain('ok-btn--ghost');
    // No Save, Save as or dirty state any more.
    expect(screen.queryByRole('button', { name: /^Save/ })).toBeNull();
  });

  it('truncates a long name with an ellipsis in a fixed width shorter than 32 characters', async () => {
    const style = document.createElement('style');
    style.textContent = readFileSync(join(process.cwd(), 'src/app.css'), 'utf8');
    document.head.append(style);
    try {
      const h = setup();
      await create(h, LONG);
      render(<App/>);
      const name = current().querySelector<HTMLElement>('.lp-current__name')!;
      expect(name.textContent).toBe(LONG);
      const css = getComputedStyle(name);
      expect([css.overflow, css.textOverflow, css.whiteSpace]).toEqual(['hidden', 'ellipsis', 'nowrap']);
      // The button's own label box must be allowed to shrink, or the name is clipped
      // without an ellipsis and the chevron is pushed out.
      expect(parseFloat(getComputedStyle(name.parentElement!).minWidth)).toBe(0);
      const width = parseFloat(getComputedStyle(current()).width);
      expect(width).toBeGreaterThan(0);
      // 32 characters of a 13px UI font need well over 200px; the trigger stays narrower.
      expect(width).toBeLessThan(32 * 6.5);
    } finally { style.remove(); }
  });

  it('lists profiles in the backend order with full names as tooltips, and switching is one undoable step', async () => {
    const h = setup();
    const defaultId = h.fixture.state().active_profile_id!;
    await create(h, 'Work');
    await create(h, LONG);
    render(<App/>);
    expect(button('Undo').disabled).toBe(true);
    const items = await openMenu();
    expect(items.map(i => i.textContent)).toEqual(profilesOf(h));
    expect(items.map(i => i.textContent)).toEqual([LONG, 'Work', 'Default layout']);
    expect(items.map(i => i.getAttribute('title'))).toEqual([LONG, 'Work', 'Default layout']);
    expect(items.map(i => i.getAttribute('aria-checked'))).toEqual(['true', 'false', 'false']);
    await act(async () => { fireEvent.click(items[2]); });
    await h.settle();
    expect(h.of('load_layout_profile').map(r => r.params!.profile_id)).toEqual([defaultId]);
    expect(current().getAttribute('aria-label')).toBe('Current layout: Default layout');
    const op = h.fixture.state().operation!;
    expect(op).toMatchObject({ kind: 'load', status: 'applied', history_required: true });
    // The list follows recency: the switched-to profile comes first.
    expect(profilesOf(h)).toEqual(['Default layout', LONG, 'Work']);

    await act(async () => { fireEvent.click(button('Undo')); });
    await h.settle();
    expect(h.of('replay_arrangement')[0].params).toMatchObject({ direction: 'undo', original_kind: 'load', from: op.after, target: op.before });
    expect(current().getAttribute('aria-label')).toBe(`Current layout: ${LONG}`);
    expect(button('Undo').disabled).toBe(true);
    await act(async () => { fireEvent.click(button('Redo')); });
    await h.settle();
    expect(h.of('replay_arrangement')[1].params).toMatchObject({ direction: 'redo', original_kind: 'load' });
    expect(current().getAttribute('aria-label')).toBe('Current layout: Default layout');
    expect(h.of('commit_arrangement')).toHaveLength(0);
  });

  it('records a manual edit as an edit that replays into the current profile', async () => {
    const h = setup(false);
    render(<App/>);
    await act(async () => { fireEvent.keyDown(document.body, { key: 'Tab' }); });
    const gamma = Array.from(document.querySelectorAll<HTMLElement>('.scr:not(.scr--group)')).find(s => s.getAttribute('aria-label')!.startsWith('Gamma,'))!;
    fireEvent.pointerDown(gamma, { button: 0, clientX: 600, clientY: 300 }); fireEvent.pointerUp(gamma);
    await act(async () => { fireEvent.keyDown(document.body, { key: 'ArrowDown' }); });
    await h.settle();
    expect(h.of('commit_arrangement')).toHaveLength(1);
    await act(async () => { fireEvent.click(button('Undo')); });
    await h.settle();
    expect(h.of('replay_arrangement')[0].params).toMatchObject({ original_kind: 'edit' });
  });

  it('keeps a partial switch as one entry and names the failed owner', async () => {
    const h = setup();
    const defaultId = h.fixture.state().active_profile_id!;
    await create(h, 'Work');
    edit(h, s => { s.placements[1].origin.y += 1; s.disabled[0].disabled = true; });
    h.fixture.failNextOwner(A);
    render(<App/>);
    const items = await openMenu();
    await act(async () => { fireEvent.click(items.find(i => i.textContent === 'Default layout')!); });
    await h.settle();
    expect(h.of('load_layout_profile').map(r => r.params!.profile_id)).toEqual([defaultId]);
    expect(h.fixture.state().operation?.status).toBe('partial');
    expect(await screen.findByText('“Default layout” was applied to only some devices')).toBeTruthy();
    expect(screen.getByText(/Alpha: The system didn’t accept the new display arrangement/)).toBeTruthy();
    expect(button('Undo').disabled).toBe(false);
  });
});

describe('creating a profile', () => {
  it('validates the name in the dialog, shows backend refusals inline and records no canvas history', async () => {
    const h = setup();
    render(<App/>);
    fireEvent.click(button('New layout'));
    const dialog = await screen.findByRole('dialog', { name: 'New layout' });
    const input = within(dialog).getByRole('textbox', { name: 'Layout name' });
    const submit = within(dialog).getByRole('button', { name: 'Create' }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    for (const [value, problem] of [['   ', 'Enter a layout name'], ['x'.repeat(33), 'The name can have at most 32 characters'],
      ['a\u0007b', 'The name can’t contain control characters'], [' DEFAULT layout ', 'A layout with this name already exists']]) {
      fireEvent.change(input, { target: { value } });
      expect(within(dialog).getByText(problem)).toBeTruthy();
      expect(submit.disabled).toBe(true);
    }
    // 32 characters are allowed, counted as characters, not bytes.
    fireEvent.change(input, { target: { value: '中'.repeat(32) } });
    expect(submit.disabled).toBe(false);
    // A backend refusal stays in the dialog with the typed name.
    const call = h.fixture.transport.call;
    h.fixture.transport.call = (async (request: { method: string; params?: Record<string, unknown> }) => request.method === 'create_layout_profile'
      ? { type: 'profile_error', error: { code: 'stale_state', message: 'stale' } } : call(request)) as typeof call;
    fireEvent.change(input, { target: { value: '  Desk  ' } });
    await act(async () => { fireEvent.click(submit); });
    expect(within(dialog).getByText('The layout changed elsewhere. Try again.')).toBeTruthy();
    expect((input as HTMLInputElement).value).toBe('  Desk  ');
    h.fixture.transport.call = call;
    await act(async () => { fireEvent.click(submit); });
    expect(h.of('create_layout_profile').map(r => r.params!.name)).toEqual(['Desk', 'Desk']);
    await waitFor(() => expect(screen.queryByRole('dialog', { name: 'New layout' })).toBeNull());
    expect(current().getAttribute('aria-label')).toBe('Current layout: Desk');
    expect(button('Undo').disabled).toBe(true);
  });

  it('disables + at sixteen profiles', async () => {
    const h = setup();
    render(<App/>);
    for (let i = 0; i < 14; i++) await create(h, `P${i}`);
    expect(button('New layout').disabled).toBe(false);
    await create(h, 'P14');
    expect(h.fixture.state().profiles).toHaveLength(16);
    expect(h.fixture.state().create_enabled).toBe(false);
    expect(button('New layout').disabled).toBe(true);
  });
});

describe('Settings › Layout profiles', () => {
  const openSettings = async (container: HTMLElement) => {
    fireEvent.click(container.querySelector('[data-action="settings"]')!);
    return await waitFor(() => container.querySelector<HTMLElement>('[data-item="manageLayouts"]')!);
  };
  const names = (row: HTMLElement) => within(row).getAllByRole('listitem').map(i => i.querySelector('.lpm__name')?.textContent);

  it('lists profiles by recency with the current one marked, renames inline and deletes after confirmation', async () => {
    const h = setup();
    await create(h, 'Work');
    const { container } = render(<App/>);
    const row = await openSettings(container);
    expect(names(row)).toEqual(['Work', 'Default layout']);
    expect(within(row).getAllByRole('listitem')[0].textContent).toContain('Current layout');
    expect(within(row).getAllByRole('listitem')[0].querySelector('.lpm__name')!.getAttribute('title')).toBe('Work');

    fireEvent.click(within(row).getByRole('button', { name: 'Rename “Default layout”' }));
    const input = within(row).getByRole('textbox', { name: 'Layout name' }) as HTMLInputElement;
    fireEvent.change(input, { target: { value: 'work' } });
    expect(within(row).getByText('A layout with this name already exists')).toBeTruthy();
    fireEvent.change(input, { target: { value: ' Home ' } });
    await act(async () => { fireEvent.click(within(row).getByRole('button', { name: 'Save' })); });
    expect(h.of('rename_layout_profile').map(r => r.params!.name)).toEqual(['Home']);
    expect(names(row)).toEqual(['Work', 'Home']);

    // Deleting the current profile selects the next by recency without a switch.
    fireEvent.click(within(row).getByRole('button', { name: 'Delete “Work”' }));
    const ask = await screen.findByRole('alertdialog', { name: 'Delete layout “Work”?' });
    fireEvent.click(within(ask).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull());
    expect(h.of('delete_layout_profile')).toHaveLength(0);
    fireEvent.click(within(row).getByRole('button', { name: 'Delete “Work”' }));
    await act(async () => { fireEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Delete' })); });
    await waitFor(() => expect(names(row)).toEqual(['Home']));
    expect(h.fixture.state().profiles[0].active).toBe(true);
    expect(h.of('load_layout_profile')).toHaveLength(0);
    expect(current().getAttribute('aria-label')).toBe('Current layout: Home');

    // The last profile cannot be deleted.
    expect((within(row).getByRole('button', { name: 'Delete “Home”' }) as HTMLButtonElement).disabled).toBe(true);
  });

  it('creates from Settings through the same dialog', async () => {
    const h = setup();
    const { container } = render(<App/>);
    const row = await openSettings(container);
    fireEvent.click(within(row).getByRole('button', { name: 'New layout' }));
    const dialog = await screen.findByRole('dialog', { name: 'New layout' });
    fireEvent.change(within(dialog).getByRole('textbox', { name: 'Layout name' }), { target: { value: 'Studio' } });
    await act(async () => { fireEvent.click(within(dialog).getByRole('button', { name: 'Create' })); });
    expect(h.of('create_layout_profile').map(r => r.params!.name)).toEqual(['Studio']);
    await waitFor(() => expect(names(row)).toEqual(['Studio', 'Default layout']));
  });

  it('opens from the toolbar menu', async () => {
    setup();
    const { container } = render(<App/>);
    await openMenu();
    await act(async () => { fireEvent.click(screen.getByRole('menuitem', { name: 'Manage layouts…' })); });
    await waitFor(() => expect(container.querySelector('[data-item="manageLayouts"]')).not.toBeNull());
  });
});

describe('a system-arranged computer on the canvas', () => {
  const alpha = () => Array.from(document.querySelectorAll<HTMLElement>('.scr:not(.scr--group)')).filter(s => s.getAttribute('aria-label')!.startsWith('Alpha,'));
  const select = (el: HTMLElement) => { fireEvent.pointerDown(el, { button: 0, clientX: 600, clientY: 300 }); fireEvent.pointerUp(el); };

  it.each([[true, 'dashed computer, solid screen'], [false, 'one solid computer ring']])('with the setting %s shows %s', (sync) => {
    setup(sync);
    render(<App/>);
    const [main, side] = alpha();
    select(side);
    const group = document.querySelector<HTMLElement>(`.scr--group[data-group="${A}"]`)!;
    expect(group).not.toBeNull();
    expect(group.hasAttribute('data-screen-selected')).toBe(sync);
    expect(side.hasAttribute('data-selected')).toBe(sync);
    expect(main.hasAttribute('data-selected')).toBe(false);
    expect(side.getAttribute('aria-pressed')).toBe('true');
    expect(main.getAttribute('aria-pressed')).toBe(String(!sync));
  });

  it('drags one screen through coalesced backend previews and commits the final plan token', async () => {
    const h = setup(true);
    render(<App/>);
    fireEvent.click(button('Auto-compact'));
    const [, side] = alpha();
    select(side);
    const zoom = cameraStore.get()!.zoom;
    fireEvent.pointerDown(side, { button: 0, clientX: 600, clientY: 300 });
    await act(async () => { await new Promise(r=>setTimeout(r,0)); });
    expect(h.of('preview_layout_projection').some(r=>!!r.params?.arrangement && r.params?.ended===false)).toBe(true);
    fireEvent.pointerMove(side, { clientX: 600, clientY: 300 + 0.2 * 4096 * zoom });
    fireEvent.pointerMove(side, { clientX: 600, clientY: 300 + 0.3 * 4096 * zoom });
    await act(async () => { fireEvent.pointerUp(side); });
    await h.settle();
    expect(h.of('preview_layout_projection').at(-1)?.params?.ended).toBe(false);
    const previews = h.of('preview_system_arrangement');
    expect(previews.length).toBeGreaterThan(0);
    expect(previews.every(p => p.params!.device === A && (p.params!.moved_screen as { display: string }).display === 'side')).toBe(true);
    const last = previews.at(-1)!.reply as PreviewReply;
    const [commit] = h.of('commit_arrangement').map(r => r.params!);
    expect(commit.native_plan_tokens).toEqual([last.plan_token]);
    // With compaction off, the committed transform is exactly the snapped preview's.
    expect((commit.placements as Placement[]).find(p => p.device === A)).toEqual(last.placements.find(p => p.device === A));
    expect(h.fixture.state().snapshot!.native.find(n => n.device === A)!.screens.find(s => s.screen.display === 'side')!.origin).toEqual({ x: 1200, y: 300 });

    await act(async () => { fireEvent.click(button('Undo')); });
    await h.settle();
    expect(h.of('replay_arrangement')[0].params).toMatchObject({ direction: 'undo', native_restore_devices: [A] });
    expect(h.fixture.state().snapshot!.native.find(n => n.device === A)!.screens.find(s => s.screen.display === 'side')!.origin).toEqual({ x: 1200, y: 0 });
  });

  it('compacts the whole computer after a native drag and undoes both in one step', async () => {
    const h = setup(true);
    const before = structuredClone(h.fixture.state().snapshot!);
    render(<App/>);
    const [, side] = alpha();
    select(side);
    const zoom = cameraStore.get()!.zoom;
    fireEvent.pointerDown(side, { button: 0, clientX: 600, clientY: 300 });
    fireEvent.pointerMove(side, { clientX: 600, clientY: 300 + 0.3 * 4096 * zoom });
    await act(async () => { fireEvent.pointerUp(side); });
    await h.settle();
    const last = h.of('preview_system_arrangement').at(-1)!.reply as PreviewReply;
    const [commit] = h.of('commit_arrangement').map(r => r.params!);
    const snapped = last.placements.find(p => p.device === A)!, final = (commit.placements as Placement[]).find(p => p.device === A)!;
    // The native drag remains as previewed; compaction translates both siblings
    // right together to close the gap to the next computer.
    expect(final.scale).toBe(snapped.scale);
    expect(final.origin).toEqual({ x: .5, y: 0 });
    expect(snapped.origin).toEqual({ x: 0, y: 0 });
    expect(h.fixture.state().snapshot!.native.find(n => n.device === A)!.screens).toEqual(last.native.screens);
    expect(h.of('preview_system_arrangement').every(p => (p.params!.moved_screen as { display: string }).display === 'side')).toBe(true);
    expect(h.of('commit_arrangement')).toHaveLength(1);
    expect(commit.native_plan_tokens).toEqual([last.plan_token]);
    expect((commit.placements as Placement[]).filter(p => p.device !== A)).toEqual(before.placements.filter(p => p.device !== A));
    await act(async () => { fireEvent.click(button('Undo')); });
    await h.settle();
    expect(h.fixture.state().snapshot!.placements).toEqual(before.placements);
    expect(h.fixture.state().snapshot!.native.map(n => n.screens)).toEqual(before.native.map(n => n.screens));
    expect(button('Undo').hasAttribute('disabled')).toBe(true);
    await act(async () => { fireEvent.click(button('Redo')); });
    await h.settle();
    expect(h.fixture.state().snapshot!.placements.find(p => p.device === A)).toEqual(final);
    expect(h.fixture.state().snapshot!.native.find(n => n.device === A)!.screens).toEqual(last.native.screens);
  });

  it('compacts the whole synced computer after resizing from a selected screen', async () => {
    const h = setup(true);
    const before = structuredClone(h.fixture.state().snapshot!);
    render(<App/>);
    const [, side] = alpha();
    select(side);
    await act(async () => { fireEvent.keyDown(document.body, { key: 'ArrowRight', metaKey: true }); });
    await h.settle();
    const commits = h.of('commit_arrangement');
    expect(commits).toHaveLength(1);
    expect(commits[0].params!.native_plan_tokens).toEqual([]);
    const after = h.fixture.state().snapshot!;
    expect(after.native.map(n => n.screens)).toEqual(before.native.map(n => n.screens));
    const moved = after.placements.find(p => p.device === A)!;
    expect(moved.scale).toBeCloseTo(.0011, 12);
    expect(moved.origin.x + 2000 * moved.scale).toBeCloseTo(2.5, 8);
    expect(after.placements.filter(p => p.device !== A)).toEqual(before.placements.filter(p => p.device !== A));
    await act(async () => { fireEvent.click(button('Undo')); });
    await h.settle();
    expect(h.fixture.state().snapshot!.placements).toEqual(before.placements);
    expect(button('Undo').hasAttribute('disabled')).toBe(true);
  });

  it('nudges a selected screen from the keyboard through the same preview and commit', async () => {
    const h = setup(true);
    render(<App/>);
    const [, side] = alpha();
    select(side);
    await act(async () => { fireEvent.keyDown(document.body, { key: 'ArrowDown' }); });
    await h.settle();
    // Selection presses preview immediately; the key nudge uses its own final plan.
    expect(h.of('preview_system_arrangement')).toHaveLength(2);
    expect(h.of('commit_arrangement')[0].params!.native_plan_tokens).toEqual([(h.of('preview_system_arrangement').at(-1)!.reply as PreviewReply).plan_token]);
  });

  it('keeps rigid editing when the setting is off', async () => {
    const h = setup(false);
    render(<App/>);
    const [, side] = alpha();
    select(side);
    const zoom = cameraStore.get()!.zoom;
    fireEvent.pointerDown(side, { button: 0, clientX: 600, clientY: 300 });
    fireEvent.pointerMove(side, { clientX: 600, clientY: 300 + 0.3 * 4096 * zoom });
    await act(async () => { fireEvent.pointerUp(side); });
    await h.settle();
    expect(h.of('preview_system_arrangement')).toHaveLength(0);
    expect(h.of('commit_arrangement')[0].params!.native_plan_tokens).toEqual([]);
  });
});


/** Alpha's card exposes the screen action, so its menu can hide and show its screens. */
function adoptScreenActions() {
  const next = clone(cards);
  const action = { kind: 'invoke', command: 'set_screen_mode', token: 'screen-toggle' } as const;
  const resource = { configuredMode: 'shared', icon: 'shared', fault: {code:'none',unknownCode:null,problem:null}, condition:'ready', action } as const;
  next.cards[0].status = {
    actions: { screenShared:action,screenLocal:action,mouseShared:action,mouseLocal:action,keyboardShared:action,keyboardLocal:action,keyboardDisabled:action,keyboardEnabled:action,clipboardShared:action,clipboardLocal:action,forwardingEnabled:action,forwardingDisabled:action },
    cursor:{icon:'local',focusDevice:A,focusName:'Alpha',lockedDevice:null,context:'group'},
    screen:resource,mouse:resource,keyboard:{...resource,disabled:false},clipboard:resource,
    speaker:{...resource,configuredMode:null,icon:'unknown',makeExitAction:action,forwardingControlsToken:null,localOutput:null,forwardingEnabled:null,activity:'unknown',receiverDevice:null,receiverName:null,activeSources:null,receiveReady:null,forwarding:null},
    game:{preference:null,active:null,detected:null,actions:{auto:action,on:action,off:action}},
  };
  agent.adopt('cards', next);
}

it('device-card screen sharing is one auto-saved edit with undo and redo', async () => {
  const h = setup();
  adoptScreenActions();
  const {container} = render(<App/>);
  const card = container.querySelector(`[data-ctx="device:${A}"]`)!;
  fireEvent.contextMenu(card, {clientX:100,clientY:100});
  const hide = await screen.findByRole('menuitem', {name:'Hide all screens'});
  await act(async () => { fireEvent.click(hide); });
  await h.settle();
  expect(h.of('commit_arrangement')).toHaveLength(1);
  expect(h.of('invoke_device_card')).toHaveLength(0);
  expect(h.of('commit_arrangement')[0].params).toMatchObject({native_plan_tokens:[],disabled:[],screen_shared:[{device:A,shared:false}]});
  expect(h.fixture.layout().geometry.screens.filter(s=>s.key.device===A).every(s=>s.device_disabled)).toBe(true);
  await act(async () => { fireEvent.click(button('Undo')); }); await h.settle();
  expect(button('Undo').disabled).toBe(true);
  expect(h.of('replay_arrangement')[0].params).toMatchObject({ direction: 'undo', original_kind: 'edit' });
  expect(h.fixture.layout().geometry.screens.filter(s=>s.key.device===A).every(s=>!s.device_disabled)).toBe(true);
  await act(async () => { fireEvent.click(button('Redo')); }); await h.settle();
  expect(h.fixture.layout().geometry.screens.filter(s=>s.key.device===A).every(s=>s.device_disabled)).toBe(true);
  // Auto-saved: switching away and back restores the hidden screens.
  expect(h.requests.some(r => r.method.startsWith('save_layout_profile'))).toBe(false);
});

it('a screen context hides just that screen through the undoable manual edit boundary', async () => {
  const h = setup();
  const {container} = render(<App/>);
  const count = container.querySelectorAll('.scr').length;
  const tile = container.querySelector<HTMLElement>('.scr')!;
  const label = tile.getAttribute('aria-label');
  fireEvent.contextMenu(tile, {clientX:100,clientY:100});
  await act(async () => { fireEvent.click(await screen.findByRole('menuitem', {name:'Hide this screen'})); });
  await h.settle();
  expect(h.of('commit_arrangement')).toHaveLength(1);
  const request = h.of('commit_arrangement')[0].params!;
  expect(request.disabled).toHaveLength(1);
  expect(request.disabled).toMatchObject([{disabled:true}]);
  expect(request.native_plan_tokens).toEqual([]);
  expect(request.screen_shared ?? []).toEqual([]);
  expect(container.querySelectorAll('.scr')).toHaveLength(count - 1);
  await act(async () => { fireEvent.click(button('Undo')); }); await h.settle();
  expect(container.querySelectorAll('.scr')).toHaveLength(count);
  expect(container.querySelector(`[aria-label="${label}"]`)).toBeTruthy();
});

it('a screen context offers only hiding that screen', async () => {
  setup();
  const {container} = render(<App/>);
  fireEvent.contextMenu(container.querySelector<HTMLElement>('.scr')!, {clientX:100,clientY:100});
  const menu = await screen.findByRole('menu');
  expect(within(menu).getAllByRole('menuitem').map(i => i.textContent)).toEqual(['Hide this screen']);
});

it('one hidden screen turns the device item into show all, which unhides it in one undoable edit', async () => {
  const h = setup();
  adoptScreenActions();
  const {container} = render(<App/>);
  fireEvent.contextMenu(container.querySelector<HTMLElement>('.scr')!, {clientX:100,clientY:100});
  await act(async () => { fireEvent.click(await screen.findByRole('menuitem', {name:'Hide this screen'})); });
  await h.settle();
  const hiddenKey = (h.of('commit_arrangement')[0].params!.disabled as {screen: {device: string; display: string}}[])[0].screen;
  const card = () => container.querySelector(`[data-ctx="device:${hiddenKey.device}"]`)!;
  // One hidden screen is enough to offer Show all, and the sidebar marks the device.
  expect(card().querySelector('.dev__hidden')?.getAttribute('aria-label')).toBe('1 screen hidden');
  fireEvent.contextMenu(card(), {clientX:100,clientY:100});
  const show = await screen.findByRole('menuitem', {name:'Show all screens'});
  expect(screen.queryByRole('menuitem', {name:'Hide all screens'})).toBeNull();
  expect(screen.queryByRole('menuitem', {name:'Show screen'})).toBeNull();
  await act(async () => { fireEvent.click(show); });
  await h.settle();
  const commits = h.of('commit_arrangement');
  expect(commits).toHaveLength(2);
  expect(commits[1].params).toMatchObject({native_plan_tokens:[], disabled:[{screen:hiddenKey, disabled:false}]});
  expect(card().querySelector('.dev__hidden')).toBeNull();
  expect(h.fixture.layout().geometry.screens.every(s => !s.hidden && !s.device_disabled)).toBe(true);
  await act(async () => { fireEvent.click(button('Undo')); }); await h.settle();
  expect(h.fixture.layout().geometry.screens.filter(s => s.hidden).map(s => s.key)).toEqual([hiddenKey]);
});

it('show all screens turns device sharing back on together with hidden screens', async () => {
  const h = setup();
  adoptScreenActions();
  edit(h, s => { s.disabled.find(d => d.screen.device === A && d.screen.display === 'side')!.disabled = true; s.screen_shared = [{device:A, shared:false}]; });
  act(() => agent.adopt('layout', h.fixture.layout()));
  const {container} = render(<App/>);
  const card = () => container.querySelector(`[data-ctx="device:${A}"]`)!;
  expect(card().querySelector('.dev__hidden')?.getAttribute('aria-label')).toBe('2 screens hidden');
  fireEvent.contextMenu(card(), {clientX:100,clientY:100});
  await act(async () => { fireEvent.click(await screen.findByRole('menuitem', {name:'Show all screens'})); });
  await h.settle();
  const commits = h.of('commit_arrangement');
  expect(commits).toHaveLength(1);
  expect(commits[0].params).toMatchObject({native_plan_tokens:[], screen_shared:[{device:A, shared:true}], disabled:[{screen:{device:A, display:'side'}, disabled:false}]});
  expect(h.fixture.layout().geometry.screens.filter(s => s.key.device === A).every(s => !s.hidden && !s.device_disabled)).toBe(true);
  await act(async () => { fireEvent.click(button('Undo')); }); await h.settle();
  const back = h.fixture.layout().geometry.screens.filter(s => s.key.device === A);
  expect(back.every(s => s.device_disabled)).toBe(true);
  expect(back.find(s => s.key.display === 'side')?.hidden).toBe(true);
});

it('edits the sole visible screen independently when its synced native sibling is hidden', async () => {
  const h = setup(true);
  const initial = structuredClone(h.fixture.state().snapshot!);
  initial.disabled.find(d => d.screen.device === A && d.screen.display === 'side')!.disabled = true;
  h.fixture.replaceCurrent(initial);
  agent.adopt('layout', h.fixture.layout());
  render(<App/>);
  fireEvent.click(button('Auto-compact')); // isolate the manual native edit
  const main = document.querySelector<HTMLElement>('.scr:not(.scr--group)[aria-label^="Alpha,"]')!;
  fireEvent.pointerDown(main, { button: 0, clientX: 600, clientY: 300 });
  fireEvent.pointerUp(main);
  const zoom = cameraStore.get()!.zoom;
  fireEvent.pointerDown(main, { button: 0, clientX: 600, clientY: 300 });
  fireEvent.pointerMove(main, { clientX: 600, clientY: 300 + .1 * 4096 * zoom });
  await act(async () => { fireEvent.pointerUp(main); });
  await h.settle();
  expect(h.of('preview_system_arrangement').length).toBeGreaterThan(0);
  const after = h.fixture.state().snapshot!, p = after.placements.find(p => p.device === A)!;
  const sibling = after.native.find(n => n.device === A)!.screens.find(s => s.screen.display === 'side')!;
  expect(p.origin.y).toBeCloseTo(.1, 9);
  expect(p.origin.y + sibling.origin.y * p.scale).toBeCloseTo(0, 9);
  expect(h.of('commit_arrangement')).toHaveLength(1);
});
