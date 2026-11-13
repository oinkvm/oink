/** Latency display shared by the device list and the route submenu. */

/** Device-card thresholds: <20 ms good, 20–50 ms fair, >50 ms poor. */
export function signalLevel(latency: number): 1 | 2 | 3 {
    return latency < 20 ? 3 : latency <= 50 ? 2 : 1;
}

export type LatencyTone = 'good' | 'fair' | 'poor';

/** The colour tier of a measured latency; `undefined` without a measurement. */
export function latencyTone(latency: number | null): LatencyTone | undefined {
    if (latency === null) return undefined;
    const level = signalLevel(latency);
    return level === 3 ? 'good' : level === 2 ? 'fair' : 'poor';
}

/** Largest number shown; anything slower reads as this. */
export const LATENCY_CAP_MS = 999;

/** Whole milliseconds as shown, rounded, at most `LATENCY_CAP_MS`. */
export function latencyNumber(latency: number): number {
    return Math.min(LATENCY_CAP_MS, Math.round(latency));
}
