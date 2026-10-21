import { useEffect, useState } from 'react';
import { Button, Switch } from 'oink-ui';
import { api, shell } from '../api/client';
import { openUpdateDialog } from '../window/Upgrade';
import { agent } from '../api/store';
import type { UpdatePreferences as Preferences, UpdateStatus } from '../api/update';
import { tr } from '../i18n';

const RELEASES='https://github.com/oinkvm/oink/releases';

/**
 * Settings → Check for updates. The inline result answers only a check the
 * user started in this Settings session: it is never derived from the
 * backend's checked_at, and clears when Settings closes or the window hides.
 * A found update opens the update dialog instead.
 */
export function UpdateCheck({status,open}:{status:UpdateStatus|null;open:boolean}) {
  const [checking,setChecking]=useState(false);
  // The check reply has been adopted; the result arrives when the backend leaves Checking.
  const [replied,setReplied]=useState(false);
  const [result,setResult]=useState<'current'|'failed'|null>(null);
  useEffect(()=>{if(!open)setResult(null);},[open]);
  useEffect(()=>{
    const hidden=()=>{if(document.hidden)setResult(null);};
    document.addEventListener('visibilitychange',hidden);
    return ()=>document.removeEventListener('visibilitychange',hidden);
  },[]);
  useEffect(()=>{
    if(!replied||!status||status.phase==='checking')return;
    setReplied(false);setChecking(false);
    if(status.phase==='failed'||status.error)setResult('failed');
    else if(status.candidate)openUpdateDialog();
    else setResult('current');
  },[replied,status]);
  const check=async()=>{
    setResult(null);
    if(!status||status.phase==='disabled'){await shell.openUrl(RELEASES).catch(()=>agent.fail('unavailable'));return;}
    setChecking(true);
    try{await agent.query('update',()=>api.update({type:'check'}));setReplied(true);}
    catch{setChecking(false);setResult('failed');}
  };
  return <span className="setp__btns">
    {result&&<span className="setp__size" role="status" data-update-result="">{tr(result==='current'?'已是最新版本':'更新失败')}</span>}
    <Button size="sm" loading={checking} disabled={checking} onClick={()=>{void check();}}>{tr('检查更新')}</Button>
  </span>;
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
        checks_enabled:status.checks_enabled,...patch,
      }}));
    } catch { agent.fail('unavailable'); }
    finally { setPending(false); }
  };
  return <div className="setp__update">
    <label>{tr('自动检查更新')} <Switch aria-label={tr('自动检查更新')} checked={status.checks_enabled} disabled={disabled} onChange={checks_enabled=>{void save({checks_enabled});}}/></label>
  </div>;
}
