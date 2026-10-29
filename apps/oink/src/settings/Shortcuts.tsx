import { tr } from '../i18n';
import { useEffect, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';
import { VisuallyHidden, animate } from 'oink-ui';
import type { Platform } from '../window/DesktopWindow';
export type ShortcutId = 'microphoneMute' | 'recall' | 'lock' | 'lockEscape' | 'pauseSelf' | 'pauseAll' | 'game' | `move:${string}`;
export interface ShortcutPref {
    on: boolean;
    keys: string;
}
export const SHORTCUTS: Array<{
    id: ShortcutId;
    label: string;
    desc: string;
    keywords: string;
}> = [
    { id: 'recall', get label() {
            return tr('召回指针到本机');
        }, get desc() {
            return tr('把指针从其他设备拉回本机屏幕中央');
        }, get keywords() { return tr('光标 鼠标 拉回 回来'); } },
    { id: 'lock', get label() {
            return tr('锁定/解锁指针到当前设备');
        }, get desc() {
            return tr('把指针锁定在当前设备；再按一次解除');
        }, get keywords() { return tr('光标 鼠标 锁定 解锁'); } },
    { id: 'lockEscape', get label() { return tr('指针锁定逃脱键'); },
        get desc() { return tr('按住此键可临时将指针移出锁定的屏幕'); },
        get keywords() { return tr('光标 鼠标 锁定 解锁'); } },
    { id: 'pauseSelf', get label() {
            return tr('暂停本机共享');
        }, get desc() {
            return tr('暂停 / 恢复本机的共享');
        }, get keywords() { return tr('暂停 恢复 停止'); } },
    { id: 'pauseAll', get label() {
            return tr('暂停设备组共享');
        }, get desc() {
            return tr('暂停设备组的共享');
        }, get keywords() { return tr('暂停 恢复 停止 全部 全组 设备组 所有可达设备 暂停全组共享'); } },
    { id: 'microphoneMute', get label() { return tr('静音共享麦克风'); },
      get desc() { return tr('切换设备组共享麦克风的静音状态'); },
      get keywords() { return tr('麦克风 静音'); } },
    { id: 'game', get label() {
            return tr('当前设备的游戏模式');
        }, get desc() {
            return tr('在开启和关闭之间切换当前设备的游戏模式');
        }, get keywords() { return tr('游戏 手柄 3D'); } },
];
export const DEFAULT_SHORTCUTS: Record<ShortcutId, ShortcutPref> = {
    microphoneMute: { on: false, keys: 'Control+Alt+M' },
    recall: { on: true, keys: 'Control+Alt+H' },
    lock: { on: true, keys: 'Control+Alt+L' },
    lockEscape: { on: true, keys: 'Alt' },
    pauseSelf: { on: true, keys: 'Control+Alt+P' },
    pauseAll: { on: true, keys: 'Control+Alt+Shift+P' },
    game: { on: true, keys: 'Control+Alt+G' },
};
const actions = {toggle_microphone_mute:'microphoneMute',return_pointer_home:'recall',lock_focus:'lock',lock_escape:'lockEscape',toggle_sharing:'pauseSelf',pause_all:'pauseAll',toggle_game_mode:'game'} as const;
export function shortcutId(h: import('../api/types').Hotkey): ShortcutId | undefined {
    return h.action === 'move_pointer_to' && h.device ? `move:${h.device}` : actions[h.action as keyof typeof actions];
}
/** Hotkeys contain precisely current roster targets, including self and offline members. */
export function deviceShortcuts(hotkeys: import('../api/types').Hotkey[], members: ReadonlyArray<{device:string;name:string}>): typeof SHORTCUTS {
    return hotkeys.filter(h=>h.action==='move_pointer_to' && h.device).map(h=>({
        id: `move:${h.device}`,
        label: tr('将指针移动到 {0}', members.find(m=>m.device===h.device)?.name ?? h.device!),
        desc: tr('把指针移动到这台设备的屏幕'),
        keywords: tr('光标 鼠标 拉回 回来'),
    }));
}
const MODS = ['Control', 'Alt', 'Shift', 'Meta'] as const;
type Mod = typeof MODS[number];
const isMod = (k: string): k is Mod => (MODS as readonly string[]).includes(k);
const heldMods = (e: {
    ctrlKey: boolean;
    altKey: boolean;
    shiftKey: boolean;
    metaKey: boolean;
}) => MODS.filter((m) => ({ Control: e.ctrlKey, Alt: e.altKey, Shift: e.shiftKey, Meta: e.metaKey })[m]);
export function mainKey(e: {
    code: string;
    key: string;
}) {
    const code = e.code || '';
    let key = code.startsWith('Key') ? code.slice(3) : code.startsWith('Digit') ? code.slice(5) : code || e.key;
    if (key.length === 1)
        key = key.toUpperCase();
    return key;
}
const LABELS: Record<Platform, Record<string, string>> = {
    mac: { Control: '⌃', Alt: '⌥', Shift: '⇧', Meta: '⌘', Space: 'Space', ArrowUp: '↑', ArrowDown: '↓', ArrowLeft: '←', ArrowRight: '→', Enter: '↩', Backspace: '⌫', Escape: 'Esc' },
    win: { Control: 'Ctrl', Alt: 'Alt', Shift: 'Shift', Meta: 'Win', Space: 'Space', ArrowUp: '↑', ArrowDown: '↓', ArrowLeft: '←', ArrowRight: '→', Enter: 'Enter', Backspace: 'Backspace', Escape: 'Esc' },
};
const SPOKEN: Record<Platform, Record<string, string>> = {
    mac: { Control: 'Control', Alt: 'Option', Shift: 'Shift', Meta: 'Command' },
    win: { Control: 'Ctrl', Alt: 'Alt', Shift: 'Shift', Meta: 'Windows' },
};
const ORDER: Record<Platform, Mod[]> = { mac: ['Control', 'Alt', 'Shift', 'Meta'], win: ['Meta', 'Control', 'Alt', 'Shift'] };
export const splitKeys = (v: string) => v.split('+').filter(Boolean);
const sortKeys = (p: Platform, keys: string[]) => [...ORDER[p].filter((m) => keys.includes(m)), ...keys.filter((k) => !isMod(k))];
export const spokenKeys = (p: Platform, v: string) => sortKeys(p, splitKeys(v)).map((k) => SPOKEN[p][k] ?? LABELS[p][k] ?? k).join(' + ');
export function matches(e: KeyboardEvent, v: string) {
    const keys = splitKeys(v);
    const main = keys.find((k) => !isMod(k));
    if (!main || mainKey(e) !== main)
        return false;
    const held = heldMods(e);
    return held.length === keys.length - 1 && held.every((m) => keys.includes(m));
}
function Cap({ label, index }: {
    label: string;
    index: number;
}) {
    const ref = useRef<HTMLElement>(null);
    useEffect(() => {
        animate(ref.current, [{ transform: 'scale(.3) translateY(8px)', opacity: 0 }, { transform: 'none', opacity: 1 }], { spring: 'bouncy', delay: index * 45 });
    }, []);
    return <kbd ref={ref} className="ok-kbd">{label}</kbd>;
}
export function ShortcutRecorder({ platform, value, onChange, onReject, disabled, invalid, label, singleKey = false }: {
    platform: Platform;
    singleKey?: boolean;
    value: string;
    onChange: (v: string) => void;
    onReject: (reason: string) => void;
    disabled?: boolean;
    invalid?: boolean;
    label: string;
}) {
    const [recording, setRecording] = useState(false);
    const [mods, setMods] = useState<string[]>([]);
    const [pointer, setPointer] = useState(false);
    const keys = splitKeys(value);
    const stop = () => { setRecording(false); setMods([]); };
    const start = () => { if (!disabled && !recording) {
        setRecording(true);
        setMods([]);
    } };
    const onKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
        if (disabled || e.repeat)
            return;
        if (!recording) {
            if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                start();
            }
            return;
        }
        if (!singleKey && e.key === 'Tab') {
            stop();
            return;
        }
        e.preventDefault();
        e.stopPropagation();
        e.nativeEvent.stopImmediatePropagation?.();
        if (singleKey) {
            const key = isMod(e.key) ? e.key : mainKey(e);
            stop();
            onChange(key);
            return;
        }
        if (e.key === 'Escape') {
            stop();
            return;
        }
        const held = heldMods(e);
        if (isMod(e.key)) {
            setMods(held);
            return;
        }
        stop();
        if (!held.length) {
            onReject(tr('至少要带一个修饰键'));
            return;
        }
        onChange([...held, mainKey(e)].join('+'));
    };
    const shown = sortKeys(platform, recording ? mods : keys);
    const hint = recording ? (mods.length ? '…' : tr(singleKey ? '按下单个按键' : '按下组合键')) : keys.length ? '' : tr('未设置');
    return (<div role="button" tabIndex={disabled ? -1 : 0} className="ok-hotkey ok-hotkey--sm ok-focusable setp__hotkey" aria-label={tr('{0}的快捷键', label)} aria-disabled={disabled || undefined} aria-pressed={recording} aria-invalid={invalid || undefined} data-recording={recording ? '' : undefined} data-pointer={pointer ? '' : undefined} onPointerDown={() => setPointer(true)} onClick={start} onKeyDown={onKeyDown} onKeyUp={(e) => { if (!recording) return;
        if (!singleKey) setMods(heldMods(e)); }} onBlur={() => { setPointer(false); if (recording)
        stop(); }}>
      {shown.map((k, i) => <Cap key={`${k}-${recording}`} label={LABELS[platform][k] ?? k} index={i}/>)}
      {hint && <span className="ok-hotkey__hint">{hint}</span>}
      <VisuallyHidden aria-live="polite">{recording ? tr(singleKey ? '正在录制，按下单个按键' : '正在录制，按下组合键') : keys.length ? tr('当前：{0}', spokenKeys(platform, value)) : tr('未设置')}</VisuallyHidden>
    </div>);
}
