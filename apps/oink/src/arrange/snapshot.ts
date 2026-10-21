import type { Layout, ScreenKey } from '../api/types';
import type { ArrangementSnapshot, NativeState, ProfileError, ProfileName } from '../api/layoutProfiles';
import { tr } from '../i18n';

const screenKey = (s: ScreenKey) => `${s.device}:${s.display}`;
const byKey = <T>(key: (v: T) => string) => (a: T, b: T) => key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0;

/** History identity: canonical content plus the active profile. Native tokens and
 * other observational revisions are ignored, so an authoritative echo of the same
 * arrangement does not count as a change. */
export function sameArrangement(a: ArrangementSnapshot | null, b: ArrangementSnapshot | null): boolean {
  if (!a || !b) return a === b;
  const canonical = (s: ArrangementSnapshot) => JSON.stringify([
    s.active_profile_id,
    [...s.placements].sort(byKey(p => p.device)).map(p => [p.device, p.origin.x, p.origin.y, p.scale]),
    [...s.disabled].sort(byKey(d => screenKey(d.screen))).map(d => [screenKey(d.screen), d.disabled]),
    [...s.native].sort(byKey(n => n.device)).map(n => [n.device, screenKey(n.primary),
      [...n.screens].sort(byKey(x => screenKey(x.screen))).map(x => [screenKey(x.screen), x.origin.x, x.origin.y, x.size.w, x.size.h])]),
    [...s.placements].sort(byKey(p => p.device)).map(p => [p.device, s.screen_shared?.find(d => d.device === p.device)?.shared ?? true]),
    s.topology,
  ]); // JSON writes -0 as 0.
  return canonical(a) === canonical(b);
}

/** The live layout drawn with a history snapshot's native geometry. A snapshot carries
 * native origins for owners with system arrangement on; rigid owners report the same
 * bounds the live layout already has. Screens the snapshot does not know keep theirs. */
export function nativeLayout(layout: Layout, native: readonly NativeState[] | undefined, snapshot?: ArrangementSnapshot | null): Layout {
  if (!native?.length) return layout;
  const bounds = new Map(native.flatMap(n => n.screens.map(s => [screenKey(s.screen), { ...s.origin, ...s.size }] as const)));
  return { ...layout, geometry: { ...layout.geometry, screens: layout.geometry.screens.map(s => {
    const next = bounds.get(screenKey(s.key));
    return { ...s, ...(next ? { display: { ...s.display, bounds: next } } : {}),
      hidden: snapshot?.disabled.find(d => screenKey(d.screen) === screenKey(s.key))?.disabled ?? s.hidden,
      device_disabled: snapshot ? !(snapshot.screen_shared?.find(d => d.device === s.key.device)?.shared ?? true) : s.device_disabled };
  }) } };
}

export const profileName = (name: ProfileName) => name.kind === 'default' ? tr('默认布局') : name.value;

/** Profile names: trimmed, nonempty, at most 32 characters, no control characters,
 * and unique case-insensitively among `taken` (the default counts as `Default layout`).
 * Checked before submission so the dialog explains it; the backend validates again
 * and its `invalid_name` / `duplicate_name` refusals are shown the same way. */
export const NAME_LIMIT = 32;
export function nameProblem(raw: string, taken: readonly string[] = []): string | null {
  const name = raw.trim();
  if (!name) return tr('请输入布局名称');
  if ([...name].length > NAME_LIMIT) return tr('名称最多 {0} 个字符', NAME_LIMIT);
  if (/[\u0000-\u001f\u007f-\u009f]/u.test(name)) return tr('名称不能包含控制字符');
  if (taken.some(t => t.toLocaleLowerCase() === name.toLocaleLowerCase())) return tr('已有同名布局');
  return null;
}

export function profileErrorText(error: Pick<ProfileError, 'code'>): string {
  switch (error.code) {
    case 'stale_state': return tr('布局已在别处更改，请重试');
    case 'profile_not_found': return tr('这个布局已被删除');
    case 'profile_limit': return tr('最多保存 16 个布局');
    case 'duplicate_name': return tr('已有同名布局');
    case 'last_profile': return tr('至少保留一个布局');
    case 'invalid_name': return tr('布局名称无效');
    case 'invalid_geometry': case 'disconnected': return tr('屏幕必须保持相邻相连');
    case 'no_valid_placement': return tr('找不到可以放下屏幕的位置');
    case 'size_limit': return tr('布局太大，无法保存');
    case 'facts_unavailable': return tr('还没读到设备的屏幕信息');
    case 'owner_offline': return tr('设备离线');
    case 'unsupported': return tr('设备不支持调整系统屏幕排列');
    case 'setting_disabled': return tr('设备没有允许修改系统屏幕排列');
    case 'busy': return tr('上一个排列操作还没完成');
    case 'not_member': return tr('设备已不在设备组中');
    case 'persistence_failed': return tr('无法保存到磁盘');
    case 'os_apply_failed': return tr('系统没有接受新的屏幕排列');
    case 'native_changed': return tr('设备的屏幕已经变化');
    case 'outcome_unknown': return tr('无法确认结果');
  }
}
