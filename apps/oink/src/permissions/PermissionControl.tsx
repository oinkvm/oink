import { Icon } from 'oink-ui';
import { tr } from '../i18n';
import type { PermissionId } from '../settings/prefs';
import type { PermState } from '../settings/SettingsPage';
import type { useGrant } from '../settings/useGrant';
import { PermissionButton } from './PermissionRow';

export function PermissionControl({ id, state, grant }: { id: PermissionId; state: PermState; grant: ReturnType<typeof useGrant> }) {
  if (state === 'restart_required') return <span className="setp__note">{id === 'files' ? tr('文件服务不可用，请重新打开 Oink') : tr('已授权；输入尚未恢复，请重新打开 Oink')}</span>;
  if (state === 'unknown' && id !== 'audio') return <span className="setp__note">{tr('不可用')}</span>;
  if (state === 'unsupported') return <span className="setp__note">{tr('此设备不支持')}</span>;
  if (state === 'not_required') return <span className="setp__note">{tr('无需授权')}</span>;
  if (state === 'granted') return <span className="ob-granted setp__granted"><Icon name="check-circle"/>{tr('已授权')}</span>;
  const fallback = grant.requested.has(id) && grant.canOpenSettings;
  return <PermissionButton variant="accent" loading={grant.pending.has(id)} onClick={() => grant.grant(id)}>
    {id === 'files' ? tr('启用') : fallback || (id === 'network' && state === 'denied') ? tr('打开设置') : tr('前往授权')}
  </PermissionButton>;
}
