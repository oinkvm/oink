import { afterEach, expect, it } from 'vitest';
import { sectionsFor } from './schema';
import { DEFAULT_SHORTCUTS } from './Shortcuts';
import { setLanguage } from '../i18n';

afterEach(() => setLanguage('en'));
it('offers the remote Windows lock only on Windows sources', () => {
    setLanguage('zh-Hans');
    const rows = (p: 'mac' | 'win') => sectionsFor(p).find(s => s.id === 'shortcuts')!.items;
    const row = rows('win').find(s => s.label === '锁定远程 Windows 计算机');
    expect(row).toBeDefined();
    expect(row?.desc).toBe('按下此快捷键，锁定指针所在 Windows 计算机，Win+L 保持锁定自身');
    expect(rows('mac').some(s => s.label === row?.label)).toBe(false);
    expect(Object.values(DEFAULT_SHORTCUTS)).toContainEqual({ on: true, keys: 'Meta+Shift+L' });
});
it.each(['en', 'ja', 'ko'] as const)('translates the remote Windows lock row in %s', language => {
    setLanguage(language);
    const row = sectionsFor('win').find(s => s.id === 'shortcuts')!.items.find(s => s.id === 'sc-remoteWindowsLock');
    expect(row).toBeDefined();
    expect(row?.label).not.toBe('锁定远程 Windows 计算机');
    expect(row?.desc).toContain('Win+L');
});
