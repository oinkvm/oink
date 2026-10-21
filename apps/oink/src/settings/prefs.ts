import type { Platform } from '../window/DesktopWindow';
import { DEFAULT_SHORTCUTS } from './Shortcuts';
import type { ShortcutId, ShortcutPref } from './Shortcuts';
export type MacKey = 'control' | 'option' | 'command' | 'shift' | 'caps';
export type WinKey = 'ctrl' | 'win' | 'alt' | 'shift' | 'caps';
export type KeyPreset = 'mac' | 'win' | 'custom';
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
        shift: { name: 'shift', sym: '⇧' }, caps: { name: 'caps lock', sym: '⇪' },
    },
    win: { ctrl: { name: 'Ctrl' }, win: { name: 'Win' }, alt: { name: 'Alt' }, shift: { name: 'Shift' }, caps: { name: 'Caps Lock' } },
};
export function presetMap(platform: Platform, preset: Exclude<KeyPreset, 'custom'>, capsIme: boolean): KeyMap {
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
    notificationLevel: import("../api/cards").UiPreferences["notification_level"];
    escStop: boolean;
    upgradeHint: boolean;
    language: string;
    keyPreset: KeyPreset;
    keyCustom: Record<Platform, KeyMap>;
    keyCustomTouched: Record<Platform, boolean>;
    capsSplit: boolean;
    pollLimit: '125' | 'none';
    scrollInvert: boolean;
    pointerGain: number;
    keepAwake: boolean;
    touchpadInject: boolean;
    macScrollFeel: boolean;
    forwardInjectedInput: boolean;
    pointerTrail: boolean;
    clipReceive: boolean;
    clipSend: boolean;
    clipText: boolean;
    clipRich: boolean;
    clipImage: boolean;
    clipFiles: boolean;
    clipPrivate: boolean;
    audioDevice: string;
    keepLocalPlayback: boolean;
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
    notificationLevel: 'default',
    escStop: true,
    upgradeHint: true,
    language: 'auto',
    keyPreset: 'mac',
    keyCustom: { mac: presetMap('mac', 'mac', false), win: presetMap('win', 'win', false) },
    keyCustomTouched: { mac: false, win: false },
    capsSplit: false,
    pollLimit: '125',
    scrollInvert: false,
    pointerGain: 1,
    keepAwake: true,
    touchpadInject: true,
    macScrollFeel: true,
    forwardInjectedInput: false,
    pointerTrail: true,
    clipReceive: true,
    clipSend: true,
    clipText: true,
    clipRich: true,
    clipImage: true,
    clipFiles: true,
    clipPrivate: true,
    audioDevice: 'default',
    keepLocalPlayback: false,
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
