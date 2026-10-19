// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { setLanguage } from '../i18n';
import { formatLastSeen } from './data';

const MIN = 60_000, HOUR = 60 * MIN, DAY = 24 * HOUR;
const now = new Date(2026, 5, 15, 0, 30).getTime();
afterEach(() => setLanguage('en'));

describe.each(['en', 'zh-Hans'] as const)('last-seen time in %s', language => {
  it.each([
    [-MIN, 'Just now', '刚刚'],
    [0, 'Just now', '刚刚'],
    [MIN - 1, 'Just now', '刚刚'],
    [MIN, '1 minute ago', '1 分钟前'],
    [2 * MIN - 1, '1 minute ago', '1 分钟前'],
    [2 * MIN, '2 minutes ago', '2 分钟前'],
    [HOUR - 1, '59 minutes ago', '59 分钟前'],
    [HOUR, '1 hour ago', '1 小时前'],
    [2 * HOUR, '2 hours ago', '2 小时前'],
    [DAY - 1, '23 hours ago', '23 小时前'],
    [DAY, '1 day ago', '1 天前'],
    [2 * DAY - 1, '1 day ago', '1 天前'],
    [2 * DAY, '2 days ago', '2 天前'],
  ] as const)('formats an elapsed %s ms', (elapsed, en, zh) => {
    setLanguage(language);
    expect(formatLastSeen(now - elapsed, now)).toBe(language === 'en' ? en : zh);
  });

  it('distinguishes an unknown timestamp from a recent one', () => {
    setLanguage(language);
    expect(formatLastSeen(null, now)).toBe(language === 'en' ? 'Unknown' : '未知');
  });
});
