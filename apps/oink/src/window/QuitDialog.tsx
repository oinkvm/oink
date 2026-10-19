import { tr } from '../i18n';
import { Button, Dialog } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
export function QuitDialog({open,container,onCancel,onQuit,pendingOperations=0}:{open:boolean;container:HTMLElement|null;onCancel:()=>void;onQuit:()=>void;pendingOperations?:number}) {
  return <Dialog {...({contained:true} as Partial<DialogProps>)} open={open} onOpenChange={value=>{if(!value)onCancel();}} container={container} size={360} title={tr('要退出 Oink 吗？')} description={tr('本机与网络中的设备将无法控制彼此，也无法共享音频和剪贴板。')} actions={<><Button onClick={onCancel}>{tr('取消')}</Button><Button variant="danger" onClick={onQuit}>{tr('退出')}</Button></>}>
    {pendingOperations>0&&<p>{tr('还有 {0} 项操作未完成，退出会中断它们。', pendingOperations)}</p>}
  </Dialog>;
}
