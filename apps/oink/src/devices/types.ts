export type DeviceOS = 'mac' | 'win' | 'android' | 'ios' | 'linux' | 'other';
/** The backend's OS name (pairing views carry it as a plain string). */
export const deviceOs = (os: string | null | undefined): DeviceOS => os === 'macos' ? 'mac' : os === 'windows' ? 'win' : os === 'android' || os === 'ios' || os === 'linux' ? os : 'other';
export const isMobile = (d: { os: DeviceOS }) => d.os === 'android' || d.os === 'ios';
export type DeviceGroup = 'self' | 'online' | 'offline';
export type CursorState = 'local' | 'remote' | 'locked' | 'unknown';
export type ShareState = 'shared' | 'local' | 'fault' | 'unknown';
export type ScreenState = 'shared' | 'disabled' | 'unknown' | 'fault';
export type GameMode = 'auto' | 'on' | 'off';
export type KeyboardState = 'shared' | 'local' | 'disabled' | 'fault' | 'unknown';
export interface SpeakerState {
    activity?: import('../api/cards').SpeakerCard['activity'];
    /** Configured role survives a capture/playback fault icon. */
    configuredMode?: 'unused' | 'source' | 'receiver' | null;
    waitingForExit?: boolean;
    selectedExitUnavailable?: boolean;
    exitUnknown?: boolean;
    canMakeExit?: boolean;
    mode: 'unused' | 'exit' | 'forwarding' | 'fault' | 'unknown';
    volume: number;
    volumeKnown?: boolean;
    mutedKnown?: boolean;
    canSetVolume?: boolean;
    muted: boolean;
    /** Local mode: the device's own system output volume (0..100) and mute, when its card reports them. */
    localOutput?: { volume: number; muted: boolean } | null;
}
export interface MobileEndpoint { enabled: boolean; canStart: boolean }
export interface Device {
    id: string;
    name: string;
    os: DeviceOS;
    /** Mobile facts are supplied by the v2 device snapshot; unknown is valid. */
    formFactor?: 'phone' | 'tablet' | 'foldable' | 'unknown';
    /** Android only: privileged mode is on and running (DeviceFacts key 32),
     * so the badge wears the crown. Offline devices never do. */
    privileged?: boolean;
    /** Phones only. `enabled`: the phone is the exit / sender in the global
     * audio configuration (its card mode); a PC may always turn it back to
     * local ("切回本地", a card audio mode). `canStart`: this PC may start the
     * endpoint (an Android phone, AudioEndpointState). */
    mobileAudio?: { speaker: MobileEndpoint; microphone: MobileEndpoint; forward?: MobileEndpoint };
    protocolCompatible?: boolean;
    group: DeviceGroup;
    offlineReason?: 'disconnected'|'connecting'|'address_stale'|'protocol_incompatible';
    ip: string;
    port: number;
    latency: number | null;
    lastSeen: number | null;
    cursor: CursorState;
    screen: ScreenState;
    screenSharing?: boolean | null;
    /** Positive AX observation only; never infer from generic input faults. */
    accessibilityMissing?: boolean;
    mouse: ShareState | 'disabled';
    keyboard: KeyboardState;
    clipboard: ShareState;
    speaker: SpeakerState;
    microphone?: {
        mode: 'unavailable' | 'idle' | 'source' | 'receiver' | 'muted';
        permissionGranted: boolean; virtualReady: boolean; waitingForSource?: boolean;
        captureActive?: boolean; consumerActive?: boolean; receivingActive?: boolean;
        fault?: import('../api/cards').ResourceFault | null;
        volume: number; muted: boolean; canSetVolume: boolean;
    };
    inputPermissionMissing?: boolean;
    gameMode: GameMode | null;
    gameActive?: boolean | null;
    gameDetected?: boolean;
    paused?: boolean;
    pauseReason?: "none" | "local_user" | "remote_user" | "emergency" | "input_backlog" | "unknown";
    unknown?: boolean;
    canPause?: boolean;
    canDisableMouse?: boolean;
    actionability?: Partial<Record<'screen'|'mouse'|'keyboard'|'clipboard'|'speaker', 'invoke'|'open_permissions'|'unavailable'>>;
    faults?: Partial<Record<'screen' | 'mouse' | 'keyboard' | 'clipboard' | 'speaker', string>>;
}
