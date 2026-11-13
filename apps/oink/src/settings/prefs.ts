import type { Platform } from '../window/DesktopWindow';
import { DEFAULT_SHORTCUTS } from './Shortcuts';
import type { ShortcutId, ShortcutPref } from './Shortcuts';
export type MacKey = 'control' | 'option' | 'command' | 'shift' | 'caps';
export type WinKey = 'ctrl' | 'win' | 'alt' | 'shift' | 'caps';
/** Which system's feel a mapping gives; Custom is the user's own table. */
export type KeyPreset = 'mac' | 'win' | 'android' | 'custom';
/** The target system whose mapping row is edited (InputTransform row). */
export type KeyTarget = 'macos' | 'windows' | 'android';
export const defaultKeyTarget = (platform: Platform): KeyTarget => platform === 'mac' ? 'windows' : 'macos';
export const localFeel = (platform: Platform): Exclude<KeyPreset, 'custom'> => platform === 'mac' ? 'mac' : 'win';
export const targetFeel = (target: KeyTarget): Exclude<KeyPreset, 'custom'> => target === 'macos' ? 'mac' : target === 'windows' ? 'win' : 'android';
/** Destination names: macOS key names for a Mac target, PC names otherwise. */
export const targetKeys = (target: KeyTarget) => target === 'macos' ? ['control', 'option', 'command', 'shift', 'caps'] : ['ctrl', 'win', 'alt', 'shift', 'caps'];
export interface KeyMap {
    keys: Record<string, string>;
    capsLong?: string;
}
export const MAC_KEYS: MacKey[] = ['caps', 'shift', 'control', 'option', 'command'];
export const WIN_KEYS: WinKey[] = ['caps', 'shift', 'ctrl', 'win', 'alt'];
export const KEY_NAMES: Record<Platform, Record<string, {
    name: string;
    sym?: string;
}>> = {
    mac: {
        control: { name: 'control', sym: '⌃' }, option: { name: 'option', sym: '⌥' }, command: { name: 'command', sym: '⌘' },
        fn: { name: 'Fn', sym: '🌐' }, shift: { name: 'shift', sym: '⇧' }, caps: { name: 'caps lock', sym: '⇪' },
    },
    win: { ctrl: { name: 'Ctrl' }, win: { name: 'Win' }, alt: { name: 'Alt' }, shift: { name: 'Shift' }, caps: { name: 'Caps Lock' } },
};
/** The table a feel produces for one source platform and target system. The
 * Mac↔Windows rows keep their established tables; every other pair sends the
 * physical keys as labelled. Mac→Android presets replace modifiers only:
 * Mac feel swaps Command/Control; Android feel uses the Windows positions. */
export function feelMap(platform: Platform, target: KeyTarget, feel: Exclude<KeyPreset, 'custom'>, capsIme: boolean): KeyMap {
    if (feel !== 'android' && (platform === 'mac' ? target === 'windows' : target === 'macos')) return presetMap(platform, feel, capsIme);
    if (platform === 'mac' && target === 'android' && feel === 'android') return presetMap(platform, 'win', false);
    if (platform === 'mac' && target === 'android' && feel === 'mac') return { keys: { control: 'win', option: 'alt', command: 'ctrl', shift: 'shift', caps: 'caps' } };
    const keys: Record<string, string> = platform === 'mac'
        ? target === 'macos' ? { control: 'control', option: 'option', command: 'command' } : { control: 'ctrl', option: 'alt', command: 'win' }
        : target === 'macos' ? { ctrl: 'control', win: 'command', alt: 'option' } : { ctrl: 'ctrl', win: 'win', alt: 'alt' };
    return { keys: { ...keys, shift: 'shift', caps: 'caps' } };
}
export function presetMap(platform: Platform, preset: 'mac' | 'win', capsIme: boolean): KeyMap {
    if (platform === 'mac') {
        return preset === 'mac'
            ? { keys: { control: 'win', option: 'alt', command: 'ctrl', shift: 'shift', caps: capsIme ? 'shift' : 'caps' }, capsLong: capsIme ? 'caps' : undefined }
            : { keys: { control: 'ctrl', option: 'win', command: 'alt', shift: 'shift', caps: 'caps' } };
    }
    return preset === 'win'
        ? { keys: { ctrl: 'command', win: 'control', alt: 'option', shift: 'shift', caps: 'caps' } }
        : { keys: { ctrl: 'control', win: 'option', alt: 'command', shift: 'shift', caps: 'caps' } };
}
export type LogLevel = 'error' | 'warn' | 'info' | 'debug' | 'trace';
export type PermissionId = 'accessibility' | 'input' | 'network' | 'files' | 'audio';
export interface Prefs {
    launchAtLogin: boolean;
    showInMenuBar: boolean;
    confirmQuit: boolean;
    pasteAsFile: boolean;
    notificationLevel: import("../api/cards").UiPreferences["notification_level"];
    escStop: boolean;
    language: string;
    keyPreset: KeyPreset;
    keyCustom: Record<Platform, KeyMap>;
    /** Initialized Custom memory, including option-only edits before any key menu edit. */
    keyCustomTouched: Record<Platform, boolean>;
    capsSplit: boolean;
    allowFnMapping: boolean;
    pollLimit: '125' | '250' | '500' | 'none';
    scrollInvert: boolean;
    forwardWheelByLines: boolean;
    pointerGain: number;
    keepAwake: boolean;
    touchpadInject: boolean;
    nativeMultifinger: boolean;
    macScrollFeel: boolean;
    forwardInjectedInput: boolean;
    takeForegroundOnLeave: boolean;
    quickerCompat: boolean;
    syncUnlock: boolean;
    pointerTrail: boolean;
    clipReceive: boolean;
    clipSend: boolean;
    clipDedupe: boolean;
    removeDsStore: boolean;
    audioEncodingQuality: 'smooth' | 'quality' | 'lossless';
    audioDevice: string;
    keepLocalPlayback: boolean;
    echoCancellation: boolean;
    shortcuts: Record<ShortcutId, ShortcutPref>;
    logLevel: LogLevel;
    logMaxMb: string;
    diagnostics: boolean;
    crashReports: boolean;
    permissions: Record<Exclude<PermissionId, 'accessibility' | 'input'>, boolean>;
}
export const DEFAULT_PREFS: Prefs = {
    launchAtLogin: false,
    showInMenuBar: true,
    confirmQuit: true,
    pasteAsFile: true,
    notificationLevel: 'default',
    escStop: true,
    language: 'auto',
    keyPreset: 'mac',
    keyCustom: { mac: presetMap('mac', 'mac', false), win: presetMap('win', 'win', false) },
    keyCustomTouched: { mac: false, win: false },
    capsSplit: false,
    allowFnMapping: false,
    pollLimit: '125',
    scrollInvert: false,
    forwardWheelByLines: true,
    pointerGain: 1,
    keepAwake: true,
    touchpadInject: true,
    nativeMultifinger: true,
    macScrollFeel: true,
    forwardInjectedInput: true,
    takeForegroundOnLeave: false,
    quickerCompat: false,
    syncUnlock: false,
    pointerTrail: true,
    clipReceive: true,
    clipSend: true,
    clipDedupe: true,
    removeDsStore: true,
    audioEncodingQuality: 'quality',
    audioDevice: 'default',
    keepLocalPlayback: false,
    echoCancellation: true,
    shortcuts: DEFAULT_SHORTCUTS,
    logLevel: 'info',
    logMaxMb: '1024',
    diagnostics: true,
    crashReports: false,
    permissions: { network: true, files: false, audio: true },
};
export type PrefsApi = {
    pending: boolean;
    launchStatus: import('../api/types').LaunchAtLoginStatus | null;
    canEdit: (key:string) => boolean;
    presentation: import('./model').PresentationPreferences;
    report: import('./model').ReportStatus | null;
    chooseReport: (allow: boolean) => Promise<void>;
    confirmLaunchAtLogin: (enabled: boolean) => Promise<void>;
    hasOverrides: boolean;
    savePresentation: (patch: Partial<import('./model').PresentationPreferences>) => void;
    ready: boolean;
    local: import('./model').LocalPreferences | null;
    saveLocal: (patch: Partial<import('./model').LocalPreferences>) => Promise<void>;
    setPrefs: (update: (current: Prefs) => Prefs) => void;
    prefs: Prefs;
    set: <K extends keyof Prefs>(key: K, value: Prefs[K]) => void;
};
