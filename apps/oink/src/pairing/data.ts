import type { DeviceOS } from '../devices/types';
export const DEFAULT_PORT = 47470;
export interface Peer {
    id: string;
    device: string;
    compatible: boolean | null;
    problem?: string;
    name: string;
    os: DeviceOS;
    ip: string;
    port: number;
    deviceId: string;
}
export interface IncomingRequest extends Peer {
    revision: string;
    targetCount: number | null;
    sourceCount: number | null;
}
export const formatAddress = (p: Pick<Peer, 'ip' | 'port'>) => `${p.ip}:${p.port}`;
export function isIPv4(s: string) {
    const parts = s.split('.');
    return parts.length === 4 && parts.every((p) => /^\d{1,3}$/.test(p) && Number(p) <= 255);
}
