import { useEffect, useState, useSyncExternalStore } from 'react';
import type { CSSProperties } from 'react';
import { Button, Dialog, Icon, Tooltip } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { api, shell } from '../api/client';
import { agent, useAgent } from '../api/store';
import type { UpdateCommand, UpdatePhase, UpdateBlocker } from '../api/update';
import { tr, useLanguage } from '../i18n';
import type { MessageKey } from '../i18n';
/** Same lookup as ReleaseNotes: exact tag, parent tags, then English. */
export function releaseNotesForLanguage(notes:Record<string,string>&{en:string},language:string):string {
  let tag=language.replaceAll('_','-').toLowerCase();
  for(;;) {
    const key=Object.keys(notes).find(key=>key.toLowerCase()===tag);
    if(key!==undefined)return notes[key];
    const dash=tag.lastIndexOf('-');if(dash<0)break;tag=tag.slice(0,dash);
  }
  return notes.en;
}
export function updatePhaseText(phase:Exclude<UpdatePhase,'idle'>):string {
  const text:Record<Exclude<UpdatePhase,'idle'>,MessageKey>={disabled:'手动更新',checking:'正在检查',available:'有可用更新',downloading:'正在下载',verifying:'正在验证',ready:'准备安装',quiescing:'正在准备更新',installing:'正在安装',awaiting_reboot:'等待系统重启',installer_busy:'安装程序正在运行',committed:'更新已安装',rolled_back:'已恢复到先前版本',repair_required:'需要修复安装',failed:'更新失败'};
  return tr(text[phase]??'更新状态未知');
}
function blockerText(blocker:UpdateBlocker) {
  const words:Record<UpdateBlocker,MessageKey>={drag:'拖放',promise:'文件准备',file_provider:'文件提供服务',clipboard_bulk:'文件传输',remote_input:'远程输入',held_input:'按键尚未释放',router_transition:'指针切换',audio:'音频播放',microphone:'音频采集',configuration:'设置保存',arrangement:'屏幕排列',locked:'设备已锁定',suspended:'设备已休眠',session_changed:'会话已变化',owner_missing:'后台未就绪',consent:'等待确认',integration_unavailable:'系统集成不可用'};
  return tr(words[blocker]??'等待中的操作');
}

// The main window mounts one update dialog; the title-bar indicator and
// Settings → Check for updates open it.
let dialogOpen=false;
const listeners=new Set<()=>void>();
function setDialogOpen(open:boolean) {
  if(dialogOpen===open)return;
  dialogOpen=open;
  for(const listener of listeners)listener();
}
export const openUpdateDialog=()=>setDialogOpen(true);
export const closeUpdateDialog=()=>setDialogOpen(false);
const subscribe=(listener:()=>void)=>{listeners.add(listener);return()=>{listeners.delete(listener);};};
const useDialogOpen=()=>useSyncExternalStore(subscribe,()=>dialogOpen);

function useUpdateCommand() {
  const [pending,setPending]=useState(false);
  const [failed,setFailed]=useState(false);
  const send=async(command:UpdateCommand)=>{setPending(true);setFailed(false);try{await agent.query('update',()=>api.update(command));}catch{if(command.type==='install')setFailed(true);else agent.fail('unavailable');}finally{setPending(false);}};
  return {pending,send,failed};
}
const inProgress=(phase:UpdatePhase|undefined)=>phase==='downloading'||phase==='verifying';
const dialogTitle=(version:string|undefined)=>version?tr('升级到 v{0}', version):tr('更新 Oink');

// Verification (Sparkle extraction) reports its own progress from zero; the
// download is complete by then, so the bar stays full until Ready.
const shownProgress=(phase:UpdatePhase|undefined,progress:number|null|undefined)=>phase==='verifying'?100:Math.max(0,Math.min(100,Math.round(progress??0)));
/** The control's own background fills with progress; the label is repeated on
 *  the fill in the on-accent colour so it reads over both parts. */
function ProgressFill({value,children}:{value:number;children:string}) {
  return <>{children}<span className="upgrade-progress" aria-hidden="true" style={{'--upgrade-progress':`${value}%`} as CSSProperties}>{children}</span></>;
}

/** Title-bar indicator: an Update pill, or the download progress, which reopens the dialog. */
export function UpgradeButton() {
  const {update}=useAgent();const {send}=useUpdateCommand();
  const progress=inProgress(update?.phase);
  const ready=update?.phase==='ready';
  const title=dialogTitle(update?.candidate?.version);
  const shown=shownProgress(update?.phase,update?.progress);
  const progressText=update?.phase==='verifying'?updatePhaseText('verifying'):`${updatePhaseText('downloading')} ${shown}%`;
  return <Tooltip interactive={false} placement="bottom" content={progress?progressText:ready?tr('完成升级'):title}>
    {progress?<button type="button" className="upgrade upgrade--bar" aria-label={progressText} onClick={openUpdateDialog}><ProgressFill value={shown}>{update?.phase==='verifying'?tr('校验中'):`${shown}%`}</ProgressFill></button>:<Button variant="accent" size="sm" className="upgrade upgrade--pill" aria-label={ready?tr('完成升级'):title} onClick={()=>{if(update?.phase==='available')void send({type:'download'});else openUpdateDialog();}}>{ready?<Icon name="check"/>:tr('更新')}</Button>}
  </Tooltip>;
}

/** The update flow as a modal inside the main window. */
export function UpgradeDialog({container}:{container:HTMLElement|null}) {
  const {update,boot}=useAgent();const language=useLanguage();const open=useDialogOpen();const {pending,send:command,failed:rejected}=useUpdateCommand();
  const [restoredFailure,setRestoredFailure]=useState(false);
  const failed=rejected||restoredFailure&&(!update||update.phase==='idle'||update.phase==='ready');
  const send=(value:UpdateCommand)=>{setRestoredFailure(false);return command(value);};
  const dismissDialog=()=>{setRestoredFailure(false);closeUpdateDialog();};
  const changeOpen=(value:boolean)=>{if(!value)setRestoredFailure(false);setDialogOpen(value);};
  useEffect(()=>{if(boot?.open_update_failure){setRestoredFailure(true);openUpdateDialog();}},[boot?.open_update_failure]);
  const manual=update?.phase==='disabled';
  const progress=inProgress(update?.phase);
  const ready=update?.phase==='ready';
  const action=manual?<Button variant="accent" leading={<Icon name="external"/>} onClick={()=>{void shell.openUrl('https://github.com/oinkvm/oink/releases').catch(()=>agent.fail('unavailable'));}}>{tr('查看发布版本')}</Button>
    :update?.phase==='available'?<Button variant="accent" loading={pending} onClick={()=>{void send({type:'download'});}}>{tr('下载更新')}</Button>
    :ready&&update.candidate?<Button variant="accent" loading={pending} onClick={()=>{void send({type:'install',expected_build:update.candidate!.build});}}>{tr('重启升级')}</Button>
    :progress?<Button loading={pending} onClick={()=>{void send({type:'cancel'});}}><ProgressFill value={shownProgress(update?.phase,update?.progress)}>{tr('取消下载')}</ProgressFill></Button>:null;
  // Closing the dialog never sends a command. A running download continues in
  // the background (the title-bar indicator reopens the dialog), so it reads
  // Hide next to Cancel download; without an action there is nothing to cancel.
  const dismiss=progress?tr('隐藏'):action?tr('取消'):tr('关闭');
  return <Dialog {...({contained:true} as Partial<DialogProps>)} open={open} onOpenChange={changeOpen} container={container} size={400} title={dialogTitle(update?.candidate?.version)} actions={<>
    <Button onClick={dismissDialog}>{dismiss}</Button>
    {action}
  </>}>
    {/* Idle has no status line: it neither means an update is ready nor that a check ran. */}
    {manual?<p>{tr('此版本需要手动下载并安装更新。')}</p>:failed?<p role="status">{updatePhaseText('failed')}</p>:!update?<p role="status">{tr('正在读取…')}</p>:update.phase!=='idle'&&<p role="status">{updatePhaseText(update.phase)}</p>}
    {!!update?.blockers.length&&<ul>{update.blockers.map(b=><li key={b}>{blockerText(b)}</li>)}</ul>}
    {(failed||update?.error&&update.error!=='not_configured')&&<p role="alert">{tr(!failed&&update?.error==='busy'?'无法准备安装。请确认系统集成可用且没有其他安装程序运行，然后重试。':'无法完成更新，请重试或前往发布页面下载安装。')}</p>}
    {ready&&<p>{tr('安装会立即中断输入、音频和文件共享，并重启 Oink。')}</p>}
    {update?.candidate&&<ul className="upgrade-notes">{releaseNotesForLanguage(update.candidate.notes,language).split('\n').map((line,i)=><li key={i}>{line}</li>)}</ul>}
  </Dialog>;
}
