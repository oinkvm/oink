import { useEffect, useRef, useState } from 'react';
import { Button, Dialog, Icon, Tooltip, toast } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { api, shell } from '../api/client';
import { agent, useAgent } from '../api/store';
import { tr, trNodes } from '../i18n';
import type { MicrophonePluginStatus } from '../api/types';
import { PermissionButton } from '../permissions/PermissionRow';
import { ProgressFill } from '../window/ProgressFill';

export function VirtualMicrophoneDescription({windows}:{windows:boolean}) {
    return <>
        <span className="setp__microphone-description">{tr('安装后，在应用中选择 {0}，即可使用其他设备共享的麦克风',windows?'CABLE Output':'Oink Microphone')}</span>
        {windows&&<span className="setp__microphone-description">{trNodes('使用 VB-CABLE：访问{0}查看许可证条款和捐赠',
            <a className="setp__microphone-link ok-focusable" href="https://www.vb-cable.com" onClick={event=>{
                event.preventDefault();
                void shell.openUrl('https://www.vb-cable.com').catch(()=>agent.fail('unavailable'));
            }}>{tr('官网')}</a>)}</span>}
        {windows&&<span className="setp__microphone-description">{tr('安装或卸载后，请重启 Windows')}</span>}
    </>;
}

type Step=NonNullable<MicrophonePluginStatus['step']>;
type Operation='install'|'uninstall';
// Each step ends at a fixed percentage node; the backend only reports steps.
const NODES:Record<'windows'|'mac',Record<Operation,readonly (readonly [Step,number])[]>>={
    windows:{install:[['verify',10],['extract',25],['install_driver',85],['finish',100]],uninstall:[['verify',10],['uninstall_driver',85],['finish',100]]},
    mac:{install:[['verify',10],['copy_plugin',35],['restart_audio',85],['finish',100]],uninstall:[['remove_plugin',40],['restart_audio',85],['finish',100]]},
};
const TAU_MS:Record<Step,number>={verify:1000,extract:2000,install_driver:8000,uninstall_driver:5000,copy_plugin:1000,remove_plugin:1000,restart_audio:3000,finish:1000};
const JUMP_MS=200;
/** Simulated progress inside a step: eases towards 90% of the span up to the
 *  step's node, so the node itself is only reached when the backend moves on. */
export function operationProgress(windows:boolean,operation:Operation,step:Step|null|undefined,elapsedMs:number):number {
    const nodes=NODES[windows?'windows':'mac'][operation];
    const index=nodes.findIndex(([name])=>name===step);
    if(index<0)return 0;
    const from=index?nodes[index-1][1]:0;
    return from+(nodes[index][1]-from)*0.9*(1-Math.exp(-Math.max(0,elapsedMs)/TAU_MS[nodes[index][0]]));
}
/** Ease-out from the value shown when the step changed; never moves backwards. */
export function jumpProgress(target:number,from:number|null,elapsedMs:number,durationMs:number):number {
    if(from===null)return target;
    if(elapsedMs>=durationMs||target<=from)return Math.max(target,from);
    return from+(target-from)*(1-(1-elapsedMs/durationMs)**3);
}
export function stepCaption(operation:Operation,step:Step|null|undefined):string {
    switch(step){
        case 'verify': return operation==='install'?tr('正在验证安装包'):tr('正在验证');
        case 'extract': return tr('正在解压');
        case 'install_driver': return tr('正在安装驱动');
        case 'uninstall_driver': return tr('正在卸载驱动');
        case 'copy_plugin': return tr('正在复制插件');
        case 'remove_plugin': return tr('正在移除插件');
        case 'restart_audio': return tr('正在重启音频服务');
        case 'finish': return tr('正在完成');
        default: return '';
    }
}
// oink-ui scales every duration by --ok-ms, which is 0 under reduced motion
// (system setting or the app's own motion preference).
function motionScale() {
    const value=parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--ok-ms'));
    return Number.isNaN(value)?1:value;
}

function OperationProgress({windows,operation,step,startedAt,complete}:{windows:boolean;operation:Operation;step:Step|null|undefined;startedAt:number|null|undefined;complete:boolean}) {
    const [now,setNow]=useState(()=>Date.now());
    const shown=useRef(0);
    const key=complete?'complete':step??null;
    const [jump,setJump]=useState<{key:typeof key;from:number|null;at:number}>({key,from:null,at:0});
    if(jump.key!==key)setJump({key,from:shown.current,at:Date.now()});
    useEffect(()=>{
        let frame=requestAnimationFrame(function tick(){setNow(Date.now());frame=requestAnimationFrame(tick);});
        return()=>cancelAnimationFrame(frame);
    },[]);
    const target=complete?100:operationProgress(windows,operation,step,now-(startedAt??now));
    shown.current=Math.max(shown.current,jumpProgress(target,jump.from,now-jump.at,JUMP_MS*motionScale()));
    const value=Math.floor(shown.current);
    const caption=complete?stepCaption(operation,'finish'):stepCaption(operation,step);
    return <span className="setp__vmic-stack">
        <span className="setp__vmic-bar" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={value} aria-label={caption||(operation==='install'?tr('安装'):tr('卸载'))}><ProgressFill value={value}>{`${value}%`}</ProgressFill></span>
        <span className="setp__vmic-caption">{caption||' '}</span>
    </span>;
}

export function VirtualMicrophone({windows,active}:{windows:boolean;active:boolean}) {
    const status=useAgent().microphonePlugin;
    const setStatus=(next:MicrophonePluginStatus)=>agent.adopt('microphonePlugin',next);
    const root=useRef<HTMLDivElement>(null);
    const [requesting,setRequesting]=useState(false);
    const [completed,setCompleted]=useState<Operation|null>(null);
    const [confirming,setConfirming]=useState(false);
    const [restarting,setRestarting]=useState(false);
    const pending=useRef(false);
    const pollFailed=useRef(false);
    const requestSerial=useRef(0);
    const requested=useRef<Operation|null>(null);
    const lastOperation=useRef<Operation|null>(null);
    const accept=(next:MicrophonePluginStatus)=>{
        setStatus(next);
        if(pending.current&&next.operation==='idle'){
            pending.current=false;
            if(next.last_operation_succeeded===false&&!pollFailed.current){
                pollFailed.current=true;
                if(next.error_kind==='cancelled')toast.info({title:tr('已取消')});
                else toast.danger({title:tr('虚拟麦克风操作失败，请重试'),...(next.detail?{description:next.detail}:{})});
            }
        }
    };
    const failed=()=>{if(!pollFailed.current){pollFailed.current=true;toast.danger({title:tr('虚拟麦克风操作失败，请重试')});}};
    const statusOperation:Operation|null=status?.operation==='installing'?'install':status?.operation==='uninstalling'?'uninstall':null;
    const busy=requesting || !!statusOperation;
    useEffect(()=>{
        if(!active)return;
        let cancelled=false;
        const serial=requestSerial.current;
        void api.microphonePluginStatus().then(next=>{if(!cancelled&&serial===requestSerial.current)accept(next);}).catch(()=>{if(!cancelled&&serial===requestSerial.current)failed();});
        return()=>{cancelled=true;};
    },[active]);
    useEffect(()=>{
        if(!active||requesting||!status||(!status.refreshing&&status.operation==='idle'))return;
        let cancelled=false;
        let inFlight=false;
        const timer=window.setInterval(()=>{
            if(inFlight)return;
            inFlight=true;
            const serial=requestSerial.current;
            void api.microphonePluginStatus().then(next=>{if(!cancelled&&serial===requestSerial.current)accept(next);})
                .catch(()=>{if(!cancelled&&serial===requestSerial.current)failed();}).finally(()=>{inFlight=false;});
        },750);
        return()=>{cancelled=true;window.clearInterval(timer);};
    },[active,requesting,status?.operation,status?.refreshing]);
    // A successful operation shows its bar reaching 100% before the row settles.
    useEffect(()=>{
        if(statusOperation){lastOperation.current=statusOperation;return;}
        if(requesting)return;
        if(lastOperation.current&&status?.last_operation_succeeded===true)setCompleted(lastOperation.current);
        lastOperation.current=null;
    },[statusOperation,requesting,status?.last_operation_succeeded]);
    useEffect(()=>{
        if(!completed)return;
        const timer=window.setTimeout(()=>setCompleted(null),JUMP_MS*motionScale()+200);
        return()=>window.clearTimeout(timer);
    },[completed]);
    const installed=!!status && (status.state==='unmanaged'||status.state==='installed'||status.state==='version_mismatch'||status.state==='not_loaded'||status.input_loaded);
    const unmanaged=windows&&installed&&!status?.managed;
    const act=async(action:'install'|'uninstall')=>{
        if(busy)return;
        requested.current=action;
        lastOperation.current=action;
        setRequesting(true);
        pending.current=true;
        pollFailed.current=false;
        requestSerial.current++;
        try {accept(await api.microphonePluginAction(action));}
        catch {pending.current=false;failed();}
        finally {setRequesting(false);}
    };
    const progress=statusOperation??(requesting?requested.current:null)??completed;
    // Ownership markers exist only after Oink's own install; a successful
    // uninstall removes them even while a pending-removal device stays present.
    const restart=windows&&!progress&&!!status?.restart_required;
    return <div className="setp__btns" ref={root}>
        {progress?<OperationProgress windows={windows} operation={progress} step={statusOperation?status?.step:null} startedAt={status?.step_started_at_ms} complete={!statusOperation&&!requesting}/>
            :restart?<span className="setp__vmic-stack"><Button size="sm" onClick={()=>setConfirming(true)}>{tr('重启')}</Button><span className="setp__vmic-caption">{status?.last_operation!=='uninstall'?tr('重启后完成安装'):tr('重启后完成卸载')}</span></span>
            :<>
                <Tooltip interactive={false} disabled={!unmanaged} content={tr('VB-CABLE 已安装，但不由 Oink 管理')}>
                    <span><Button size="sm" disabled={!status||unmanaged||status.state==='unsupported'} onClick={()=>{void act(installed?'uninstall':'install');}}>{installed?tr('卸载'):tr('安装')}</Button></span>
                </Tooltip>
                {status?.state==='version_mismatch'&&<Button size="sm" onClick={()=>{void act('install');}}>{tr('更新')}</Button>}
            </>}
        <Dialog {...({ contained: true } as Partial<DialogProps>)} open={confirming} container={root.current?.closest<HTMLElement>('.win') ?? null}
            onOpenChange={value => { if (!restarting) setConfirming(value); }} size={440} title={tr('重新启动系统')}
            description={status?.last_operation!=='uninstall'?tr('需要重新启动系统以完成虚拟麦克风的安装，请提前保存好工作'):tr('需要重新启动系统以完成虚拟麦克风的卸载，请提前保存好工作')}
            actions={<><Button disabled={restarting} onClick={() => setConfirming(false)}>{tr('取消')}</Button><Button variant="danger" loading={restarting} disabled={restarting} onClick={() => {
                setRestarting(true);
                void api.restartSystem().catch(()=>{setConfirming(false);agent.fail('unavailable');}).finally(()=>setRestarting(false));
            }}>{tr('重新启动')}</Button></>}/>
    </div>;
}
export function MicrophonePermission() {
    const state=useAgent().permissions.find(p=>p.kind==='microphone')?.state;
    const [pending,setPending]=useState(false);
    if(state==='Granted')return <span className="ob-granted setp__granted"><Icon name="check-circle"/>{tr('已授权')}</span>;
    return <PermissionButton variant="accent" loading={pending} onClick={()=>{
        setPending(true);
        void api.requestPermission('microphone').then(()=>agent.query('permissions',api.permissions))
            .catch(()=>agent.fail('permission_denied')).finally(()=>setPending(false));
    }}>{tr('前往授权')}</PermissionButton>;
}
