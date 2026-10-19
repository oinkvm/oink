import { tr } from '../i18n';
import { useRef, useState } from 'react';
import type { FormEvent } from 'react';
import { Button, Dialog, Icon, Input } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { isIPv4 } from '../pairing/data';
import { useShake } from '../pairing/PairDialog';
import type { Device } from './types';
export function AddressDialog({ device, container, onCancel, onConfirm, pending = false, problem = null, addresses = [] }: {
    device: Device | null;
    container: HTMLElement | null;
    onCancel: () => void;
    onConfirm: (d: Device, ip: string, port: number) => void;
    pending?: boolean;
    problem?: string | null;
    addresses?: string[];
}) {
    const last = useRef<Device | null>(device);
    if (device)
        last.current = device;
    const d = last.current;
    const containedProps = { contained: true } as Partial<DialogProps>;
    return (<Dialog {...containedProps} open={!!device} onOpenChange={(o) => { if (!o)
        onCancel(); }} container={container} size={400} title={tr('更新 {0} 地址', d?.name ?? '')} actions={<>
          <Button onClick={onCancel}>{tr('取消')}</Button>
          <Button type="submit" form="addr-form" variant="accent" loading={pending} disabled={pending}>{tr('确认')}</Button>
        </>}>
      
      {d?.group === 'offline' && <p>{tr('设备已离线，仍可查看和保存地址。恢复网络后将自动重连。')}</p>}
      {addresses.length > 0 && <p>{tr('已保存的地址：{0}', addresses.join(tr('、')))}</p>}
      {problem && <p role="alert">{problem}</p>}
      {d && <AddressForm key={device ? d.id : 'closed'} d={d} onConfirm={(ip, port) => onConfirm(d, ip, port)}/>}
    </Dialog>);
}
function AddressForm({ d, onConfirm }: {
    d: Device;
    onConfirm: (ip: string, port: number) => void;
}) {
    const [ip, setIp] = useState('');
    const [port, setPort] = useState('');
    const [error, setError] = useState<{
        text: string;
        field: 'ip' | 'port';
        n: number;
    } | null>(null);
    const shake = useShake(error ? error.n : null);
    const fail = (text: string, field: 'ip' | 'port') => setError((e) => ({ text, field, n: (e?.n ?? 0) + 1 }));
    const submit = (e: FormEvent) => {
        e.preventDefault();
        const ipv = ip.trim();
        const pv = port.trim();
        if (!ipv && !d.ip) return fail(tr("请输入 IP 地址"), 'ip');
        if (ipv && !isIPv4(ipv))
            return fail(tr('请输入有效的 IPv4 地址，例如 192.168.1.20'), 'ip');
        const p = pv ? Number(pv) : d.port;
        if (!Number.isInteger(p) || p < 1 || p > 65535)
            return fail(tr('端口应在 1–65535 之间'), 'port');
        onConfirm(ipv || d.ip, p);
    };
    return (<form className="manual" id="addr-form" onSubmit={submit} noValidate>
      <div className="manual__row" ref={shake}>
        <label className="manual__field manual__field--ip">
          <span className="manual__label">{tr('IP 地址')}</span>
          <Input value={ip} onChange={(v) => {
            if (v.includes(':')) {
                fail(tr('请输入有效的 IPv4 地址'), 'ip');
                return;
            }
            setIp(v.replace(/[^\d.]/g, '').slice(0, 15));
            if (error?.field === 'ip')
                setError(null);
        }} placeholder={d.ip} inputMode="decimal" autoComplete="off" spellCheck={false} autoFocus invalid={error?.field === 'ip'} aria-describedby="addr-msg"/>
        </label>
        <label className="manual__field manual__field--port">
          <span className="manual__label">{tr('端口')}</span>
          <Input value={port} onChange={(v) => { setPort(v.replace(/\D/g, '').slice(0, 5)); if (error?.field === 'port')
        setError(null); }} placeholder={String(d.port)} inputMode="numeric" autoComplete="off" invalid={error?.field === 'port'} aria-describedby="addr-msg"/>
        </label>
      </div>
      <p className="manual__msg" id="addr-msg" data-shown={error ? '' : undefined} aria-live="polite">
        <span>{error && <><Icon name="alert"/>{error.text}</>}</span>
      </p>
    </form>);
}
