import { useState } from 'react';
import { Button, Dialog, Icon, Tooltip } from 'oink-ui';
import { api, shell } from '../api/client';
import { agent, useAgent } from '../api/store';
import type { UpdateCommand, UpdatePhase, UpdateBlocker } from '../api/update';
import { tr, useLanguage } from '../i18n';
import type { MessageKey } from '../i18n';
export function updatePhaseText(phase:UpdatePhase):string {
  const text:Record<UpdatePhase,MessageKey>={disabled:'手动更新',idle:'已就绪',checking:'正在检查',available:'有可用更新',downloading:'正在下载',verifying:'正在验证',ready:'准备安装',quiescing:'正在准备更新',installing:'正在安装',awaiting_reboot:'等待系统重启',installer_busy:'安装程序正在运行',committed:'更新已安装',rolled_back:'已恢复到先前版本',repair_required:'需要修复安装',failed:'更新失败'};
  return tr(text[phase]??'更新状态未知');
}
function blockerText(blocker:UpdateBlocker) {
  const words:Record<UpdateBlocker,MessageKey>={drag:'拖放',promise:'文件准备',file_provider:'文件提供服务',clipboard_bulk:'文件传输',remote_input:'远程输入',held_input:'按键尚未释放',router_transition:'指针切换',audio:'音频播放',microphone:'音频采集',configuration:'设置保存',arrangement:'屏幕排列',locked:'设备已锁定',suspended:'设备已休眠',session_changed:'会话已变化',owner_missing:'后台未就绪',consent:'等待确认',integration_unavailable:'系统集成不可用'};
  return tr(words[blocker]??'等待中的操作');
}
export function UpgradeButton({container,standalone=false}:{container:HTMLElement|null;standalone?:boolean}) {
  const {update}=useAgent();const language=useLanguage();const [open,setOpen]=useState(standalone),[pending,setPending]=useState(false);
  const manual=update?.phase==='disabled';
  const send=async(command:UpdateCommand)=>{setPending(true);try{await agent.query('update',()=>api.update(command));}catch{agent.fail('unavailable');}finally{setPending(false);}};
  const progress=update?.phase==='downloading'||update?.phase==='verifying';
  const ready=update?.phase==='ready';
  const title=update?.candidate?tr('升级到 v{0}', update.candidate.version):tr('更新 Oink');
  return <>
    {!standalone&&<Tooltip interactive={false} placement="bottom" content={progress?`${updatePhaseText(update!.phase)} ${Math.round(update?.progress??0)}%`:ready?tr('完成升级'):title}>
      {progress?<span className="upgrade upgrade--bar" tabIndex={0} role="progressbar" aria-label={updatePhaseText(update!.phase)} aria-valuenow={update?.progress??undefined} aria-valuemin={0} aria-valuemax={100}><span className="upgrade__fill" style={{width:`${update?.progress??0}%`}}/></span>:<Button variant="accent" size="sm" className="upgrade upgrade--pill" aria-label={ready?tr('完成升级'):title} onClick={()=>{if(update?.phase==='available')void send({type:'download'});else setOpen(true);}}>{ready?<Icon name="check"/>:tr('更新')}</Button>}
    </Tooltip>}
    <Dialog {...({contained:true} as Partial<import('oink-ui').DialogProps>)} open={open} onOpenChange={next=>{setOpen(next);if(!next&&standalone)void shell.close();}} container={container} size={400} title={title} actions={<>
      <Button onClick={()=>{setOpen(false);if(standalone)void shell.close();}}>{tr('取消')}</Button>
      {manual?<Button variant="accent" leading={<Icon name="external"/>} onClick={()=>{void shell.openUrl('https://github.com/oinkvm/oink/releases').catch(()=>agent.fail('unavailable'));}}>{tr('查看发布版本')}</Button>:update?.phase==='available'?<Button variant="accent" loading={pending} onClick={()=>{void send({type:'download'});}}>{tr('下载更新')}</Button>:update?.phase==='ready'&&update.candidate?<Button variant="accent" loading={pending} onClick={()=>{void send({type:'install',expected_build:update.candidate!.build});}}>{tr('重启升级')}</Button>:progress?<Button loading={pending} onClick={()=>{void send({type:'cancel'});}}>{tr('取消下载')}</Button>:null}
    </>}>
      {manual?<p>{tr('此版本需要手动下载并安装更新。')}</p>:<p role="status">{update?updatePhaseText(update.phase):tr('正在读取…')}</p>}
      {!!update?.blockers.length&&<ul>{update.blockers.map(b=><li key={b}>{blockerText(b)}</li>)}</ul>}
      {update?.error&&update.error!=='not_configured'&&<p role="alert">{tr(update.error==='busy'?'无法准备安装。请确认系统集成可用且没有其他安装程序运行，然后重试。':'无法完成更新，请重试或前往发布页面下载安装。')}</p>}
      {ready&&<p>{tr('安装会立即中断输入、音频和文件共享，并重启 Oink。')}</p>}
      {/* The release feed carries Simplified Chinese and English notes only; other languages show the English notes. */}
      {update?.candidate&&<ul className="upgrade-notes">{update.candidate.notes[language === 'zh-Hans' ? 'zh-Hans' : 'en'].split('\n').map((line,i)=><li key={i}>{line}</li>)}</ul>}
    </Dialog>
  </>;
}
