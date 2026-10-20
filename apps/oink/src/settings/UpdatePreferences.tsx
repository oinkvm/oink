import { useState } from 'react';
import { Select, Switch } from 'oink-ui';
import { api, shell } from '../api/client';
import { agent } from '../api/store';
import type { UpdatePreferences as Preferences, UpdateStatus } from '../api/update';
import { tr } from '../i18n';

export async function checkForUpdates(status:UpdateStatus|null) {
  if (!status || status.phase === 'disabled') {
    await shell.openUrl('https://github.com/oinkvm/oink/releases');
  } else {
    await agent.query('update',()=>api.update({type:'check'}));
    await shell.open('upgrade');
  }
}

export function UpdatePreferences({status}:{status:UpdateStatus|null}) {
  const [pending,setPending] = useState(false);
  if (!status || status.phase === 'disabled') return <span>{tr('此版本需要手动下载并安装更新。')}</span>;
  const disabled = pending || !status.can_change_policy;
  const save = async (patch:Partial<Preferences>) => {
    if (disabled) return;
    setPending(true);
    try {
      await agent.query('update',()=>api.update({type:'set_preferences',preferences:{
        checks_enabled:status.checks_enabled,channel:status.channel,...patch,
      }}));
    } catch { agent.fail('unavailable'); }
    finally { setPending(false); }
  };
  return <div className="setp__update">
    <label>{tr('自动检查更新')} <Switch aria-label={tr('自动检查更新')} checked={status.checks_enabled} disabled={disabled} onChange={checks_enabled=>{void save({checks_enabled});}}/></label>
    <Select size="sm" aria-label={tr('更新渠道')} value={status.channel} disabled={disabled} onChange={channel=>{if(channel==='stable'||channel==='beta')void save({channel});}} options={[
      {value:'stable',label:tr('稳定版')},{value:'beta',label:tr('测试版')},
    ]}/>
    <p>{tr('更新下载完成后，点击安装会立即中断共享并重启 Oink。')}</p>
    <p>{status.scope==='machine' ? tr('更新设置适用于这台电脑的所有用户；更改需要管理员权限。') : tr('更新设置仅适用于当前用户。')}</p>
  </div>;
}
