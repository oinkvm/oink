/** Pointer positions are integer percentages of the original track, before mapping.
 * Keyboard/wheel/typed values bypass this function entirely. */
export const inVolumeDetent = (raw: number, max: number) => max === 200 && raw >= 97 && raw <= 103;

export function pointerVolume(raw: number, max: number): number {
    raw = Math.max(0, Math.min(max, Math.round(raw)));
    if (max !== 200) return raw;
    if (inVolumeDetent(raw, max)) return 100;
    return raw < 97 ? Math.round(raw * 99 / 96) : Math.round(101 + (raw - 104) * 99 / 96);
}
