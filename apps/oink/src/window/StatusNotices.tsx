import { useEffect, useRef } from 'react';
import { confirm, toast } from 'oink-ui';
import { api } from '../api/client';
import { agent, useAgent } from '../api/store';
import type { AlertKind } from '../api/types';
import { problemText, tr, useLanguage } from '../i18n';
function alertText(kind:Exclude<AlertKind, {type:'permission_missing'}>):string {
  switch(kind.type){
    case 'capture_unavailable':return tr('无法采集输入');
    case 'display_info_unavailable':return tr('无法读取显示器信息');
    case 'secure_input':return tr('安全输入已启用，键盘共享暂不可用');
    case 'new_screen':return tr('检测到新屏幕，请检查排列');
    case 'group_state_unreadable':return tr('无法读取设备组状态');
    case 'peer_offline':return tr('设备已离线');
    case 'emergency_stop':return tr('已收到暂停设备组共享指令');
    default:return tr('共享状态需要注意');
  }
}
async function resetGroup() {
  if (await confirm({title:tr('重置设备组？'),description:tr('重置后需要重新配对才能与其他设备共享。'),tone:'danger',confirmText:tr('重置')})) {
    try { await api.resetGroupState(); await agent.query('overview',api.overview); }
    catch { agent.fail('unavailable'); }
  }
}

export function StatusNotices(){
  const {overview,cards}=useAgent();
  const language=useLanguage();
  const shown=useRef(new Map<string,string>());
  const coverage=cards?.emergencyDelivery;
  useEffect(()=>{
    const notices=(overview?.alerts ?? []).flatMap(alert=>{
      if(alert.kind.type==='permission_missing')return [];
      return [{
        id:`status:${alert.id}`, title:alertText(alert.kind), description:alert.problem ? problemText(alert.problem.code) : undefined,
        tone:alert.level==='error' ? 'danger' as const : alert.level==='warning' ? 'warning' as const : 'info' as const,
        duration:alert.kind.type==='group_state_unreadable' ? 0 : undefined,
        action:alert.kind.type==='group_state_unreadable' ? {label:tr('重置回初始状态'),onClick:()=>{void resetGroup();}} : undefined,
      }];
    });
    if(overview?.pause.paused && overview.pause.reason === 'input_backlog') notices.push({
      id:'status:input-backlog', title:tr('输入积压超过 5 秒，已暂停共享'), tone:'warning',
      description:tr('已丢弃积压的输入。准备好后可恢复本机共享。'), duration:0, action:undefined,
    });
    if(coverage) notices.push({
      id:'status:emergency-delivery', title:tr('暂停指令送达'), tone:'info', action:undefined, duration:undefined,
      description:[tr('{0}/{1} 已确认',coverage.recipients.filter(r=>r.state==='confirmed').length,coverage.recipients.length),tr('{0} 未确认',coverage.recipients.filter(r=>r.state==='unreachable'||r.state==='unconfirmed').length),...(coverage.recipients.some(r=>r.cleanupPending)?[tr('清理未完成')]:[])].join(' · '),
    });
    const active=new Set(notices.map(notice=>notice.id));
    for(const id of shown.current.keys()) if(!active.has(id)) {toast.dismiss(id);shown.current.delete(id);}
    for(const notice of notices){
      // Overview snapshots and events describe the same retained issue. Stable IDs
      // update a changed issue in place; unchanged snapshots do not restart a toast.
      const signature=JSON.stringify([notice.title,notice.description,notice.tone]);
      if(shown.current.get(notice.id)===signature)continue;
      shown.current.set(notice.id,signature);
      toast(notice);
    }
  },[overview?.alerts,overview?.pause,coverage,language]);
  return null;
}
