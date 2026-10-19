import { tr } from '../i18n';
import type { AccentPreset } from 'oink-ui';
export const DEFAULT_ACCENT = { value: 'pink', color: '#FF6B8E', dark: '#FF6B8E' } as const;
export const ACCENT_SWATCHES: Array<{
    value: AccentPreset;
    color: string;
    dark: string;
    label: string;
}> = [
    { ...DEFAULT_ACCENT, get label() {
            return tr('粉');
        } },
    { value: 'orange', color: '#FF7A2F', dark: '#FF7A2F', get label() {
            return tr('橙');
        } },
    { value: 'amber', color: '#FFBE3B', dark: '#FFBE3B', get label() {
            return tr('琥珀');
        } },
    { value: 'blue', color: '#2E7BFF', dark: '#2E7BFF', get label() {
            return tr('蓝');
        } },
    { value: 'violet', color: '#C77DFF', dark: '#C77DFF', get label() {
            return tr('紫');
        } },
];
