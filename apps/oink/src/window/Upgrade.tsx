import { useEffect, useState, useSyncExternalStore } from 'react';
import { Button, Checkbox, Dialog, Icon, Tooltip, toast } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { usePackageDrag, usePackageDropReceiver } from './PackageDrop';
import { ProgressFill } from './ProgressFill';
export { localPackagePath } from './PackageDrop';
import { api, shell } from '../api/client';
import { ApiFailure } from '../api/transport';
import { problemText } from '../i18n';
import { agent, useAgent } from '../api/store';
import { preferencesWith } from '../api/update';
import type { UpdateCommand, UpdatePhase, UpdateBlocker, UpdateError, UpdateStatus } from '../api/update';
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
const subscribeVisibility=(listener:()=>void)=>{document.addEventListener('visibilitychange',listener);return()=>document.removeEventListener('visibilitychange',listener);};
const useWindowVisible=()=>useSyncExternalStore(subscribeVisibility,()=>!document.hidden);

function useUpdateCommand() {
  const [pending,setPending]=useState(false);
  const [failed,setFailed]=useState(false);
  const [reason,setReason]=useState<string|null>(null);
  const send=async(command:UpdateCommand)=>{setPending(true);setFailed(false);setReason(null);try{await agent.query('update',()=>api.update(command));}catch(error){
    if(command.type==='install'||command.type==='open_local'){setFailed(true);setReason(error instanceof ApiFailure?error.updateError??error.problem.code:null);}
    else toast.danger({title:error instanceof ApiFailure&&error.updateError&&localErrors[error.updateError]?tr(localErrors[error.updateError]!):problemText(error instanceof ApiFailure?error.problem.code:'backend_failed')});
  }finally{setPending(false);}};
  return {pending,send,failed,reason};
}
const inProgress=(phase:UpdatePhase|undefined)=>phase==='downloading'||phase==='verifying';
const dialogTitle=(candidate:UpdateStatus['candidate']|undefined)=>candidate?tr('升级到 {0}', candidate.version):tr('更新 Oink');

// Verification (Sparkle extraction) reports its own progress from zero; the
// download is complete by then, so the bar stays full until Ready.
const shownProgress=(phase:UpdatePhase|undefined,progress:number|null|undefined)=>phase==='verifying'?100:Math.max(0,Math.min(100,Math.round(progress??0)));

/** Title-bar indicator: an Update pill, or the download progress. Both open the dialog, so the notes are read before downloading. */
export function UpgradeButton() {
  const {update}=useAgent();
  const progress=inProgress(update?.phase);
  const critical=update?.candidate?.kind==='critical';
  const ready=update?.phase==='ready';
  const title=dialogTitle(update?.candidate);
  const shown=shownProgress(update?.phase,update?.progress);
  const progressText=update?.phase==='verifying'?updatePhaseText('verifying'):`${updatePhaseText('downloading')} ${shown}%`;
  // A retained candidate describes the previous transaction too; idle and
  // committed states do not advertise another update.
  if (!update?.candidate || ['disabled', 'idle', 'committed'].includes(update.phase)) return null;
  return <Tooltip interactive={false} placement="bottom" content={progress?progressText:ready?tr('完成升级'):title}>
    {progress?<button type="button" className="upgrade upgrade--bar" aria-label={progressText} onClick={openUpdateDialog}><ProgressFill value={shown}>{update?.phase==='verifying'?tr('校验中'):`${shown}%`}</ProgressFill></button>:<Button variant="accent" size="sm" className={critical&&!ready?'upgrade upgrade--pill upgrade--critical':'upgrade upgrade--pill'} aria-label={ready?tr('完成升级'):title} onClick={openUpdateDialog}>{ready?<Icon name="check"/>:tr('更新')}</Button>}
  </Tooltip>;
}

const localErrors:Partial<Record<UpdateError,MessageKey>>={
  policy:'安装包确认已失效。请重新拖入安装包并重试。',
  transaction:'另一更新正在进行，或更新事务已失效。请重新确认后重试。',
  storage:'无法保存更新状态。请检查磁盘空间和写入权限。',
  not_configured:'此版本未启用在线更新。请拖入本地安装包或前往发布页面下载安装。',
  metadata:'更新请求或安装包元数据无效。请重新拖入安装包。',
  busy:'无法准备安装。请确认系统集成可用且没有其他安装程序运行，然后重试。',
  wrong_product:'此安装包不是 Oink。',wrong_architecture:'此安装包不支持这台设备的架构。',downgrade:'无法降级。请使用相同或更新的版本和构建。',
  package_unavailable:'无法读取或下载此安装包。请检查源设备和文件提供服务是否可用。',
  verification_timeout:'安装包验证超时。Oink 仍在运行，请重试。',
  corrupt_package:'安装包已损坏或不完整。',signature:'安装包签名无效或签名者不受信任。',not_notarized:'此 Mac 应用尚未通过 Apple 公证。',
  not_writable:'无法在无本机授权的情况下替换此位置的 Oink。',elevation:'管理员授权已取消或失败，Oink 仍在运行。',
};

const RELEASES='https://github.com/oinkvm/oink/releases';
// GitHub's latest-release page skips pre-releases, which the beta channel offers.
const releasesPage=(channel:UpdateStatus['channel']|undefined)=>channel==='beta'?RELEASES:`${RELEASES}/latest`;
const openReleases=(channel:UpdateStatus['channel']|undefined)=>{void shell.openUrl(releasesPage(channel)).catch(()=>agent.fail('unavailable'));};

/** The update flow as a modal inside the main window. */
export function UpgradeDialog({container}:{container:HTMLElement|null}) {
  const {update,boot}=useAgent();const language=useLanguage();const open=useDialogOpen();const {pending,send:command,failed:rejected,reason}=useUpdateCommand();
  const [restoredFailure,setRestoredFailure]=useState(false);
  const [localFile,setLocalFile]=useState(false);
  // The user started a check from this dialog; its outcome is shown here even when nothing is found.
  const [rechecked,setRechecked]=useState(false);
  const dragging=usePackageDrag();
  usePackageDropReceiver(boot?.platform==='windows'?'win':'mac',path=>{
    setLocalFile(true);setRestoredFailure(false);openUpdateDialog();
    void command({type:'open_local',path});
  });
  useEffect(()=>{if(update?.candidate?.local_package===false)setLocalFile(false);},[update?.candidate]);
  const localPackage=localFile||update?.candidate?.local_package===true;
  const failed=rejected||restoredFailure&&(!update||update.phase==='idle'||update.phase==='ready');
  const send=(value:UpdateCommand)=>{setRestoredFailure(false);return command(value);};
  const discardLocal=()=>{
    if(localPackage&&(update?.phase==='ready'||update?.phase==='failed')) {setLocalFile(false);void send({type:'cancel'});}
  };
  const dismissDialog=()=>{discardLocal();setRestoredFailure(false);setRechecked(false);closeUpdateDialog();};
  const changeOpen=(value:boolean)=>{if(!value){discardLocal();setRestoredFailure(false);setRechecked(false);}setDialogOpen(value);};
  useEffect(()=>{if(boot?.open_update_failure){setRestoredFailure(true);openUpdateDialog();}},[boot?.open_update_failure]);
  // A critical update opens its dialog by itself once, the first time the
  // window is visible with it; the backend shows the window at launch.
  const visible=useWindowVisible();
  const unprompted=update?.candidate?.kind==='critical'&&!update.candidate.local_package&&update.critical_prompted_build!==update.candidate.build?update.candidate.build:null;
  useEffect(()=>{
    if(unprompted===null||!visible)return;
    openUpdateDialog();
    void command({type:'prompted',expected_build:unprompted});
  },[unprompted,visible]);
  const manual=update?.phase==='disabled';
  const progress=inProgress(update?.phase)&&!failed;
  const ready=update?.phase==='ready';
  const candidate=update?.candidate??null;
  const kind=localPackage?'normal':candidate?.kind??'normal';
  // A failed download keeps its candidate: the dialog offers the same build
  // again. The failure line belongs to that build only.
  const downloadFailed=!failed&&!localPackage&&update?.phase==='failed'&&!!candidate&&update.failed_build===candidate.build;
  const retryDownload=!failed&&!localPackage&&update?.phase==='failed'&&!!candidate&&kind!=='manual'&&!(update.error&&localErrors[update.error]);
  // A failed check has no candidate: retry the check from here.
  const recoverable=!failed&&!localPackage&&update?.phase==='failed'&&!candidate&&!(update.error&&localErrors[update.error]);
  const offered=!localPackage&&!!candidate&&(update?.phase==='available'||retryDownload);
  const action=failed||kind==='manual'?null:recoverable?<Button variant="accent" loading={pending} onClick={()=>{setRechecked(true);void send({type:'check'});}}>{tr('检查更新')}</Button>
    :offered?<Button variant="accent" loading={pending} onClick={()=>{void send({type:'download'});}}>{tr('下载')}</Button>
    :ready&&update.candidate?<Button variant="accent" loading={pending} onClick={()=>{void send({type:'install',expected_build:update.candidate!.build});}}>{tr('重启升级')}</Button>
    :progress&&!localPackage?<Button loading={pending} onClick={()=>{void send({type:'cancel'});}}><ProgressFill value={shownProgress(update?.phase,update?.progress)}>{tr('取消下载')}</ProgressFill></Button>:null;
  // Skipping only silences automatic checks for this build; a critical or
  // manual-only route has no skip.
  const skippable=offered&&kind==='normal'&&update?.phase==='available';
  // Closing a local confirmation discards its private staged copy. A running
  // feed download continues in the background (the title-bar indicator reopens the dialog), so it reads
  // Hide next to Cancel download; without an action there is nothing to cancel.
  const dismiss=progress?tr('隐藏'):action&&!recoverable?tr('取消'):tr('关闭');
  const installAfterDownload=update?.install_after_download??true;
  const offersAutoInstall=!localPackage&&!!candidate&&kind!=='manual'&&(offered||progress);
  const setInstallAfterDownload=(value:boolean)=>{
    if(!update)return;
    void send({type:'set_preferences',preferences:preferencesWith(update,{install_after_download:value})});
  };
  const errorLine=downloadFailed?tr('下载失败，请检查网络连接后重试，或前往发布页手动下载安装包后拖拽进窗口安装。')
    :kind==='manual'&&candidate?tr('版本过低，请前往发布页手动下载更新。')
    // A network failure that belongs to another build is history: the candidate now offered has not failed.
    :!localPackage&&candidate&&update?.error==='network'&&update.failed_build!==candidate.build?null
    :recoverable&&update?.error==='network'?tr('查询新版本失败')
    :failed||update?.error&&update.error!=='not_configured'?(reason&&!localErrors[reason as UpdateError]?problemText(reason):tr(localErrors[(reason??update?.error) as UpdateError]||'无法完成更新，请重试或前往发布页面下载安装。'))
    :null;
  const notice=kind==='critical'&&candidate?.notice?releaseNotesForLanguage(candidate.notice,language):null;
  return <Dialog {...({contained:true} as Partial<DialogProps>)} open={open&&!dragging} onOpenChange={changeOpen} container={container} size={520} className="upgrade-dialog" title={dialogTitle(update?.candidate)} actions={<div className="upgrade-footer">
    <div className="upgrade-footer__row">
      <Button variant="ghost" size="sm" className="upgrade-footer__link" leading={<Icon name="external"/>} onClick={()=>openReleases(update?.channel)}>{tr('发布页')}</Button>
      <span className="upgrade-footer__actions">
        {skippable&&<Button onClick={()=>{void send({type:'skip',expected_build:candidate!.build});closeUpdateDialog();}}>{tr('跳过此版本')}</Button>}
        <Button onClick={dismissDialog}>{dismiss}</Button>
        {action}
      </span>
    </div>
    {offersAutoInstall&&<Checkbox className="upgrade-footer__auto" checked={installAfterDownload} onChange={setInstallAfterDownload}>{tr('下载完成后自动安装')}</Checkbox>}
  </div>}>
    {errorLine&&<p role="alert" className="upgrade-error">{errorLine}</p>}
    {/* Idle has no status line unless the user just checked from here: by itself it neither means an update is ready nor that a check ran.
        An offered candidate is described by the title and notes; a download by its progress button. */}
    {failed?<p role="status">{updatePhaseText('failed')}</p>:recoverable||kind==='manual'&&candidate?null:manual?<p>{tr('此版本需要手动下载并安装更新。')}</p>:!update?<p role="status">{tr('正在读取…')}</p>:update.phase==='idle'?rechecked&&!update.candidate&&<p role="status">{tr('已是最新版本')}</p>:!offered&&!['ready','available','downloading'].includes(update.phase)&&<p role="status">{updatePhaseText(update.phase)}</p>}
    {!!update?.blockers.length&&<ul>{update.blockers.map(b=><li key={b}>{blockerText(b)}</li>)}</ul>}
    {(notice||update?.candidate?.notes)&&<div className="upgrade-notes-box">
      {notice&&<p className="upgrade-notice">{notice}</p>}
      {update?.candidate?.notes&&<ul className="upgrade-notes">{releaseNotesForLanguage(update.candidate.notes,language).split('\n').filter(line=>line.trim()).map((line,i)=><li key={i}>{line}</li>)}</ul>}
    </div>}
    {localPackage&&ready&&boot?.platform==='windows'&&<p>{tr('Oink 无法验证此本地 Windows 安装包的发布者。请仅在信任此文件时继续。')}</p>}
    {ready&&<p>{tr('安装会立即中断输入、音频和文件共享，并重启 Oink。')}</p>}
  </Dialog>;
}
