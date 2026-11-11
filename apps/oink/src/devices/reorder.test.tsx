// @vitest-environment jsdom
import { act, cleanup, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { setLanguage } from '../i18n';
import { presentCards } from './project';
import { DeviceList } from './DeviceList';
import { applyPlacement, placementFor, slotAt, stepPlacement } from './reorder';
import type { DeviceCardsView } from '../api/cards';
import type { Device } from './types';
import { DEFAULT_SHORTCUTS, SHORTCUTS, shortcutId } from '../settings/Shortcuts';

beforeAll(() => {
    Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); setLanguage('en'); });
// jsdom has no PointerEvent; keep the fields the list reads.
class FakePointer extends MouseEvent {
    pointerId: number; pointerType: string;
    constructor(type: string, init: PointerEventInit = {}) { super(type, init); this.pointerId = init.pointerId ?? 1; this.pointerType = init.pointerType ?? 'mouse'; }
}

describe('online order math', () => {
    const ids = ['a', 'b', 'c', 'd'];
    it('drops above a row or after the previous last row', () => {
        expect(placementFor(ids, 'd', 0)).toEqual({ above: 'a' });
        expect(placementFor(ids, 'a', 2)).toEqual({ above: 'd' });
        expect(placementFor(ids, 'a', 3)).toEqual({ after: 'd' });
        expect(placementFor(ids, 'd', 3)).toBeNull();
        expect(placementFor(ids, 'b', 1)).toBeNull();
        expect(placementFor(['a'], 'a', 0)).toBeNull();
        expect(applyPlacement(ids, 'd', { above: 'b' })).toEqual(['a', 'd', 'b', 'c']);
        expect(applyPlacement(ids, 'a', { after: 'd' })).toEqual(['b', 'c', 'd', 'a']);
    });
    it('steps one place for Alt+arrow keys', () => {
        expect(stepPlacement(ids, 'c', -1)).toEqual({ above: 'b' });
        expect(stepPlacement(ids, 'b', 1)).toEqual({ above: 'd' });
        expect(stepPlacement(ids, 'c', 1)).toEqual({ after: 'd' });
        expect(stepPlacement(ids, 'a', -1)).toBeNull();
        expect(stepPlacement(ids, 'd', 1)).toBeNull();
        expect(slotAt([10, 30, 50], 5)).toBe(0);
        expect(slotAt([10, 30, 50], 31)).toBe(2);
        expect(slotAt([10, 30, 50], 99)).toBe(3);
    });
    it('keeps the backend card order inside each section', () => {
        const card = (device: string, isSelf: boolean, section: string) => ({ device, name: device.toUpperCase(), os: 'windows', isSelf, section,
            connection: section === 'offline' ? { kind: 'offline', reason: 'disconnected' } : { kind: 'online', signal: { rttMs: 1 } }, lastSeenMs: null, status: null, pause: null });
        const view = { cards: [card('me', true, 'this_device'), card('z', false, 'online'), card('a', false, 'online'), card('y', false, 'offline'), card('b', false, 'offline')] } as unknown as DeviceCardsView;
        expect(presentCards(view, []).map(d => `${d.group}:${d.id}`)).toEqual(['self:me', 'online:z', 'online:a', 'offline:y', 'offline:b']);
    });
});

const device = (id: string, group: Device['group']): Device => ({
    id, name: id.toUpperCase(), os: 'win', group, ip: '', port: 0, latency: null, lastSeen: null,
    cursor: 'remote', screen: 'shared', mouse: 'shared', keyboard: 'shared', clipboard: 'shared',
    speaker: { mode: 'unused', volume: 0, muted: false }, gameMode: null,
});
const devices = [device('me', 'self'), device('a', 'online'), device('b', 'online'), device('c', 'online'), device('off', 'offline')];
function list(onReorder = vi.fn(), onSelect = vi.fn(), rows = devices) {
    const view = render(<OinkProvider><DeviceList devices={rows} selectedId={null} onSelect={onSelect} onToggle={() => {}} collapsed={false} now={0} onReorder={onReorder}/></OinkProvider>);
    const rowsIn = (group: string) => [...view.container.querySelectorAll<HTMLLIElement>(`[data-group="${group}"] .devs__list > li`)];
    // Rows are 40px tall, starting at y=0 in the online list.
    rowsIn('online').forEach((li, index) => {
        li.getBoundingClientRect = () => ({ top: index * 40, height: 40, bottom: index * 40 + 40, left: 0, right: 200, width: 200, x: 0, y: index * 40, toJSON() {} });
    });
    return { ...view, rowsIn, onReorder, onSelect };
}
const order = (rows: HTMLLIElement[]) => rows.map(li => li.dataset.id);
function drag(li: HTMLLIElement, from: number, to: number) {
    fireEvent.pointerDown(li.querySelector('.dev__hit')!, { button: 0, clientY: from });
    fireEvent.pointerMove(window, { clientY: from + 2 });
    fireEvent.pointerMove(window, { clientY: to });
    fireEvent.pointerUp(window, { clientY: to });
    fireEvent.click(li.querySelector('.dev__hit')!, { detail: 1 });
}

describe('online list press-and-drag', () => {
    it('drops above a row, sends one command, swallows the ending click and shows the new order at once', async () => {
        vi.stubGlobal('PointerEvent', FakePointer);
        const { rowsIn, onReorder, onSelect } = list(vi.fn(() => new Promise(() => {})));
        const [a, , c] = rowsIn('online');
        drag(c, 100, 10);
        expect(onReorder).toHaveBeenCalledExactlyOnceWith('c', { above: 'a' });
        expect(onSelect).not.toHaveBeenCalled();
        expect(order(rowsIn('online'))).toEqual(['c', 'a', 'b']);
        // A plain click still selects; offline and self rows are not draggable.
        fireEvent.click(a.querySelector('.dev__hit')!, { detail: 1 });
        expect(onSelect).toHaveBeenCalledWith('a');
        expect(rowsIn('offline')[0].dataset.id).toBeUndefined();
        expect(rowsIn('self')[0].dataset.id).toBeUndefined();
    });
    it('drops after the previous last online row and marks the drop slot while dragging', () => {
        vi.stubGlobal('PointerEvent', FakePointer);
        const { rowsIn, onReorder } = list();
        const [a, , c] = rowsIn('online');
        fireEvent.pointerDown(a.querySelector('.dev__hit')!, { button: 0, clientY: 10 });
        fireEvent.pointerMove(window, { clientY: 115 });
        expect(c.dataset.drop).toBe('below');
        expect(a.dataset.dragging).toBe('');
        fireEvent.pointerUp(window, { clientY: 115 });
        expect(onReorder).toHaveBeenCalledExactlyOnceWith('a', { after: 'c' });
        expect(c.dataset.drop).toBeUndefined();
    });
    it('ignores small movements, icon presses, secondary buttons, touch and Escape', () => {
        vi.stubGlobal('PointerEvent', FakePointer);
        const { rowsIn, onReorder, onSelect } = list();
        const [a, b] = rowsIn('online');
        fireEvent.pointerDown(a.querySelector('.dev__hit')!, { button: 0, clientY: 10 });
        fireEvent.pointerMove(window, { clientY: 13 });
        fireEvent.pointerUp(window, { clientY: 13 });
        fireEvent.click(a.querySelector('.dev__hit')!, { detail: 1 });
        expect(onSelect).toHaveBeenCalledWith('a');
        fireEvent.pointerDown(b.querySelector('button.dev-ico--btn')!, { button: 0, clientY: 50 });
        fireEvent.pointerMove(window, { clientY: 0 });
        fireEvent.pointerUp(window, { clientY: 0 });
        fireEvent.pointerDown(b.querySelector('.dev__hit')!, { button: 2, clientY: 50 });
        fireEvent.pointerMove(window, { clientY: 0 });
        fireEvent.pointerUp(window, { clientY: 0 });
        fireEvent.pointerDown(b.querySelector('.dev__hit')!, { button: 0, clientY: 50, pointerType: 'touch' });
        fireEvent.pointerMove(window, { clientY: 0 });
        fireEvent.pointerUp(window, { clientY: 0 });
        fireEvent.pointerDown(b.querySelector('.dev__hit')!, { button: 0, clientY: 50 });
        fireEvent.pointerMove(window, { clientY: 0 });
        fireEvent.keyDown(window, { key: 'Escape' });
        fireEvent.pointerUp(window, { clientY: 0 });
        expect(onReorder).not.toHaveBeenCalled();
    });
    it('moves the focused row with Alt+arrow keys and reverts the optimistic order on failure', async () => {
        let reject: (e: Error) => void = () => {};
        const { rowsIn, onReorder } = list(vi.fn(() => new Promise((_, r) => { reject = r; })));
        const b = rowsIn('online')[1];
        fireEvent.keyDown(b.querySelector('.dev__hit')!, { key: 'ArrowDown', altKey: true });
        expect(onReorder).toHaveBeenCalledExactlyOnceWith('b', { after: 'c' });
        expect(order(rowsIn('online'))).toEqual(['a', 'c', 'b']);
        expect(b.querySelector('.dev__hit')!.getAttribute('aria-keyshortcuts')).toBe('Alt+ArrowUp Alt+ArrowDown');
        await act(async () => { reject(new Error('refused')); });
        expect(order(rowsIn('online'))).toEqual(['a', 'b', 'c']);
        fireEvent.keyDown(rowsIn('online')[0].querySelector('.dev__hit')!, { key: 'ArrowUp', altKey: true });
        fireEvent.keyDown(rowsIn('online')[0].querySelector('.dev__hit')!, { key: 'ArrowDown' });
        expect(onReorder).toHaveBeenCalledTimes(1);
    });
    it('follows the next device cards once the backend order changes', () => {
        const { rowsIn, rerender } = list(vi.fn(() => new Promise(() => {})));
        fireEvent.keyDown(rowsIn('online')[2].querySelector('.dev__hit')!, { key: 'ArrowUp', altKey: true });
        expect(order(rowsIn('online'))).toEqual(['a', 'c', 'b']);
        const next = [devices[0], devices[3], devices[1], devices[2], devices[4]];
        rerender(<OinkProvider><DeviceList devices={next} selectedId={null} onSelect={() => {}} collapsed={false} now={0} onReorder={() => {}}/></OinkProvider>);
        expect(order(rowsIn('online'))).toEqual(['c', 'a', 'b']);
    });
});

describe('switch pointer position shortcut', () => {
    it('is listed, defaults off on Ctrl+Option/Alt+N and maps to the backend action', () => {
        setLanguage('en');
        expect(SHORTCUTS.find(s => s.id === 'pointerNext')?.label).toBe('Switch pointer position');
        expect(DEFAULT_SHORTCUTS.pointerNext).toEqual({ on: false, keys: 'Control+Alt+N' });
        expect(shortcutId({ action: 'move_pointer_next', enabled: false, keys: ['Ctrl', 'Alt', 'N'] })).toBe('pointerNext');
        for (const [lang, label] of [['zh-Hans', '切换指针位置'], ['ja', 'ポインタ位置を切り替え'], ['ko', '포인터 위치 전환']]) {
            setLanguage(lang);
            expect(SHORTCUTS.find(s => s.id === 'pointerNext')?.label).toBe(label);
        }
    });
});
