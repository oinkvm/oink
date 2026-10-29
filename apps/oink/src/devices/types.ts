export type DeviceOS = 'mac' | 'win' | 'linux' | 'other';
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
export interface Device {
    id: string;
    name: string;
    os: DeviceOS;
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
