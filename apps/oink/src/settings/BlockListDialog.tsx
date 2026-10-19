import { tr } from '../i18n';
import { Button, Dialog, Icon } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { OsLogo } from '../devices/icons';
import type { Peer } from '../pairing/data';
export function BlockListDialog({ open, blocked, container, onClose, onRemove, loading = false, pending = [], error = null }: {
    open: boolean;
    blocked: Peer[];
    container: HTMLElement | null;
    onClose: () => void;
    onRemove: (p: Peer) => void;
    loading?: boolean;
    pending?: string[];
    error?: string | null;
}) {
    const containedProps = { contained: true } as Partial<DialogProps>;
    return (<Dialog {...containedProps} open={open} onOpenChange={(o) => { if (!o)
        onClose(); }} container={container} size={420} title={tr('屏蔽名单')} description={tr('名单里的设备发来的配对请求会被自动拒绝，也不会出现在附近设备里。')} actions={<Button variant="accent" onClick={onClose}>{tr('完成')}</Button>}>
      
      {error && <p role="alert">{error}</p>}
      {loading ? <div className="blk blk--empty" role="status">{tr("正在读取…")}</div> : blocked.length === 0 && !error ? (<div className="blk blk--empty"><p className="blk__empty"><Icon name="check-circle"/>{tr('没有屏蔽的设备')}</p></div>) : (<ul className="blk ok-scroll" aria-label={tr('屏蔽的设备')}>
          {blocked.map((p) => (<li key={p.id} className="blk__item">
              <span className="blk__os"><OsLogo os={p.os}/></span>
              <span className="blk__text">
                <span className="blk__name">{p.name}</span>
                <span className="blk__id">{p.deviceId}</span>
              </span>
              <Button size="sm" className="blk__remove" loading={pending.includes(p.id)} disabled={pending.includes(p.id)} aria-label={tr('把 {0} 移出屏蔽名单', p.name)} onClick={() => onRemove(p)}>{tr('移除')}</Button>
            </li>))}
        </ul>)}
    </Dialog>);
}
