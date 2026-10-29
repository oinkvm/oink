import { useRef, useState } from 'react';
import { Button, Dialog } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { shell } from '../api/client';
import { tr } from '../i18n';

function failureText(step: unknown): string {
    if (typeof step === 'object' && step !== null && 'step' in step && step.step === 'microphone_plugin') {
        if ('kind' in step && step.kind === 'cancelled') return tr('虚拟麦克风未移除，应用尚未移入废纸篓，可重试');
        const detail = 'detail' in step && typeof step.detail === 'string' ? step.detail : '';
        return tr('移除 Oink 虚拟麦克风失败。应用尚未移入废纸篓，可重试。原因：{0}', detail);
    }
    switch (step) {
        case 'installed_app': return tr('请从 /Applications 中已安装的 Oink 打开卸载功能。');
        case 'leave_group': return tr('退出设备组失败。应用尚未移入废纸篓。');
        case 'stop_backend': return tr('停止 Oink 后台失败。应用尚未移入废纸篓。');
        case 'remove_domains': return tr('移除文件共享域失败。Oink 已停止，应用尚未移入废纸篓。请重试或重新打开 Oink。');
        case 'microphone_plugin': return tr('虚拟麦克风未移除，应用尚未移入废纸篓，可重试');
        case 'login_item': return tr('注销登录项失败。应用尚未移入废纸篓。');
        default: return tr('启动卸载收尾进程失败。应用尚未移入废纸篓。');
    }
}

export function UninstallControl() {
    const root = useRef<HTMLSpanElement>(null);
    const [open, setOpen] = useState(false);
    const [pending, setPending] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const confirm = async () => {
        if (pending) return;
        setPending(true); setError(null);
        try {
            await shell.uninstall({
                title: tr('卸载 Oink'), failure: tr('卸载过程中以下步骤未完成：'), done: tr('完成'),
                processes: tr('停止 Oink 进程'), privacy: tr('重置 Oink 隐私权限'),
                data: tr('删除 Oink 数据（受系统保护的容器可能保留）'), trash: tr('将 Oink 移入废纸篓'),
            });
        } catch (step) { setError(failureText(step)); setPending(false); }
    };
    return <span ref={root}>
        <Button size="sm" variant="danger" onClick={() => { setError(null); setOpen(true); }}>{tr('卸载 Oink')}</Button>
        <Dialog {...({ contained: true } as Partial<DialogProps>)} open={open} container={root.current?.closest<HTMLElement>('.win') ?? null}
            onOpenChange={value => { if (!pending) setOpen(value); }} size={440} title={tr('要卸载 Oink 吗？')}
            description={tr('这会移除已安装的 Oink 虚拟麦克风（需要管理员授权，会短暂中断声音和通话），停止 Oink、退出设备组，移除文件共享域和其中的全部文件、登录项、所有 Oink 数据、缓存、日志及隐私权限，并将 Oink 移入废纸篓。受系统保护的容器可能保留。此操作无法撤销。')}
            actions={<><Button disabled={pending} onClick={() => setOpen(false)}>{tr('取消')}</Button><Button variant="danger" loading={pending} disabled={pending} onClick={() => { void confirm(); }}>{tr('卸载 Oink')}</Button></>}>
            {pending && <p role="status">{tr('正在准备卸载 Oink；若已安装虚拟麦克风，将先请求管理员授权并移除。')}</p>}
            {error && <p role="alert">{error}</p>}
        </Dialog>
    </span>;
}
