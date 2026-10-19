import { tr } from '../i18n';
import { useRef } from 'react';
import { Button, Dialog } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import type { Device } from './types';
export function LeaveDialog({ device, container, onCancel, onConfirm }: {
    device: Device | null;
    container: HTMLElement | null;
    onCancel: () => void;
    onConfirm: (d: Device) => void;
}) {
    const last = useRef<Device | null>(device);
    if (device)
        last.current = device;
    const d = last.current;
    const self = d?.group === 'self';
    const containedProps = { contained: true } as Partial<DialogProps>;
    return (<Dialog {...containedProps} open={!!device} onOpenChange={(o) => { if (!o)
        onCancel(); }} container={container} size={360} title={self ? tr('离开设备组') : tr('将 {0} 踢出设备组', d?.name ?? '')} description={self ? tr('离开后，需要重新配对才能再次和其他设备共享。') : tr('踢出后，需要重新配对才能再次和它共享。')} actions={<>
          <Button onClick={onCancel}>{tr('取消')}</Button>
          <Button variant="danger" onClick={() => { if (d)
            onConfirm(d); }}>{self ? tr('离开') : tr('踢出')}</Button>
        </>}/>);
}
