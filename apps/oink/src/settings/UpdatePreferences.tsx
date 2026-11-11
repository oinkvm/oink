import { useEffect, useState } from 'react';
import { Button, Segmented, Switch, toast } from 'oink-ui';
import { ApiFailure } from '../api/transport';
import { api, shell } from '../api/client';
import { openUpdateDialog } from '../window/Upgrade';
import { agent } from '../api/store';
import { preferencesWith } from '../api/update';
import type { UpdateChannel as Channel, UpdatePreferences as Preferences, UpdateStatus } from '../api/update';
import { problemText, tr } from '../i18n';

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
    {result&&<span className="setp__size" role="status" data-update-result="">{tr(result==='current'?'已是最新版本':'查询新版本失败')}</span>}
    <Button size="sm" loading={checking} disabled={checking} onClick={()=>{void check();}}>{tr('检查更新')}</Button>
  </span>;
}

/** Builds without in-app updates: the row explains the manual route instead of a switch. */
export const manualUpdates = (status:UpdateStatus|null) => !status || status.phase === 'disabled';

/** Settings → Automatically check for updates: the row's switch. */
export function UpdatePreferences({status,label}:{status:UpdateStatus|null;label:string}) {
  const [pending,setPending] = useState(false);
  if (!status || manualUpdates(status)) return null;
  const disabled = pending || !status.can_change_policy;
  const save = async (patch:Partial<Preferences>) => {
    if (disabled) return;
    setPending(true);
    try {
      await agent.query('update',()=>api.update({type:'set_preferences',preferences:preferencesWith(status,patch)}));
    } catch { agent.fail('unavailable'); }
    finally { setPending(false); }
  };
  return <Switch aria-label={label} checked={status.checks_enabled} disabled={disabled} onChange={checks_enabled=>{void save({checks_enabled});}}/>;
}

/** Settings → Update channel. Beta also offers stable releases; leaving it never downgrades. */
export function UpdateChannel({status,label}:{status:UpdateStatus|null;label:string}) {
  const [pending,setPending] = useState(false);
  if (!status || manualUpdates(status)) return null;
  const save = async (channel:Channel) => {
    setPending(true);
    try { await agent.query('update',()=>api.update({type:'set_preferences',preferences:preferencesWith(status,{update_channel:channel})})); }
    catch(error) { toast.danger({title:problemText(error instanceof ApiFailure?error.problem.code:'backend_failed')}); }
    finally { setPending(false); }
  };
  return <Segmented<Channel> size="sm" aria-label={label} value={status.channel} disabled={pending||!status.can_change_policy} onChange={channel=>{void save(channel);}} options={[
    {value:'stable',get label(){return tr('正式版');}},
    {value:'beta',get label(){return tr('测试版');}},
  ]}/>;
}
