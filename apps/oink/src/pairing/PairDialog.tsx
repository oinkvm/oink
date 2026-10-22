import { tr, trNodes } from '../i18n';
import { useEffect, useRef, useState } from 'react';
import type { FormEvent, KeyboardEvent, ReactNode } from 'react';
import { Button, ContextMenu, MenuItem, MenuSeparator, Dialog, Dots, Icon, Input, PinInput, Spinner, Tooltip, speed, useBusy } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { NearbyRow } from './AddPanel';
import { agent } from '../api/store';
import { PairFailure, pairProblemText } from './model';
import { formatAddress, isIPv4 } from './data';
import type { Peer } from './data';
import type { PairSession, Pairing } from './usePairing';
import { Morph } from './Morph';
import './pairing.css';
export function PairDialog({ pairing, container }: {
    pairing: Pairing;
    container: HTMLElement | null;
}) {
    const { dialog, actions } = pairing;
    const s = dialog.session;
    const manual = dialog.mode === 'manual';
    const title = manual ? tr('输入 IP 配对') : s ? tr('与 {0} 配对', s.peer.name) : '';
    const failed = s?.status === 'failed';
    const containedProps = { contained: true } as Partial<DialogProps>;
    return (<Dialog {...containedProps} open={dialog.open} onOpenChange={(o) => { if (!o)
        void actions.cancel().catch(()=>agent.fail('unavailable')); }} dismissible={false} container={container} size={440} title={title} className="pairdlg" onExited={actions.exited} onKeyDown={(e: KeyboardEvent) => { if (e.key === 'Escape') {
        e.stopPropagation();
        void actions.cancel().catch(()=>agent.fail('unavailable'));
    } }} actions={<Button onClick={() => {void actions.cancel().catch(()=>agent.fail('unavailable'));}} disabled={s?.status === 'success'}>{failed ? tr('关闭') : tr('取消')}</Button>}>
      <Morph viewKey={manual ? `m${dialog.key}` : `p${s?.key ?? ''}-${failed ? 'failed' : s?.status === 'connecting' ? 'connecting' : 'code'}`} pending={s?.status === 'connecting' && s.source === 'manual'}>
        {manual ? <ManualSearch key={dialog.key} pairing={pairing}/> : s ? <PairBody session={s} pairing={pairing}/> : null}
      </Morph>
    </Dialog>);
}
export function useShake(trigger: unknown) {
    const ref = useRef<HTMLDivElement>(null);
    useEffect(() => {
        const s = speed();
        if (trigger == null || s <= 0)
            return;
        ref.current?.animate({ transform: ['translateX(0)', 'translateX(-7px)', 'translateX(6px)', 'translateX(-4px)', 'translateX(2px)', 'translateX(0)'] }, { duration: 520 * s, easing: 'cubic-bezier(.22, 1, .36, 1)' });
    }, [trigger]);
    return ref;
}
export function ManualSearch({ pairing, autoFocus = true }: {
    autoFocus?: boolean;
    pairing: Pairing;
}) {
    const { actions, defaultPort } = pairing;
    const [ip, setIp] = useState('');
    const [port, setPort] = useState('');
    const [error, setError] = useState<{
        field?: 'ip' | 'port';
        text: string;
        n: number;
    } | null>(null);
    const [found, setFoundState] = useState<Peer | null>(null);
    const [leaving, setLeaving] = useState(false);
    const leaveTimer = useRef<number | undefined>(undefined);
    useEffect(() => () => window.clearTimeout(leaveTimer.current), []);
    const setFound = (p: Peer) => { window.clearTimeout(leaveTimer.current); setLeaving(false); setFoundState(p); };
    const busy = useBusy({ successHold: 500 });
    const shake = useShake(error?.field ? error.n : null);
    const fail = (text: string, field?: 'ip' | 'port') => setError((e) => ({ text, field, n: (e?.n ?? 0) + 1 }));
    const submit = (e: FormEvent) => {
        e.preventDefault();
        if (busy.state === 'loading')
            return;
        window.clearTimeout(leaveTimer.current);setFoundState(null);setLeaving(false);
        const ipv = ip.trim();
        const pv = port.trim();
        if (!ipv)
            return fail(tr('请输入 IP 地址'), 'ip');
        if (!isIPv4(ipv))
            return fail(tr('请输入有效的 IPv4 地址，例如 192.168.1.20'), 'ip');
        const p = pv ? Number(pv) : defaultPort;
        if (!/^\d+$/.test(pv || '0') || p < 1 || p > 65535)
            return fail(tr('端口应在 1–65535 之间'), 'port');
        setError(null);
        busy.run(actions.search(ipv, p)).then(setFound, (error:unknown) => fail(error instanceof PairFailure ? pairProblemText(error.problem) : tr('连接失败，请检查地址或稍后重试')));
    };
    return (<form className="manual" onSubmit={submit} noValidate>
      <div className="manual__row" ref={shake}>
        <label className="manual__field manual__field--ip">
          <span className="manual__label">{tr('IP 地址')}<span className="manual__req" aria-hidden>*</span></span>
          <Input value={ip} onChange={(v) => {
            if (v.includes(':')) {
                fail(tr('请输入有效的 IPv4 地址'), 'ip');
                return;
            }
            setIp(v.replace(/[^\d.]/g, '').slice(0, 15));
            if (error?.field === 'ip')
                setError(null);
        }} inputMode="decimal" autoComplete="off" spellCheck={false} required autoFocus={autoFocus} invalid={error?.field === 'ip'} aria-describedby="manual-msg"/>
        </label>
        <label className="manual__field manual__field--port">
          <span className="manual__label">{tr('端口')}</span>
          <Input value={port} onChange={(v) => { setPort(v.replace(/\D/g, '').slice(0, 5)); if (error?.field === 'port')
        setError(null); }} placeholder={String(defaultPort)} inputMode="numeric" autoComplete="off" invalid={error?.field === 'port'} aria-describedby="manual-msg"/>
        </label>
        <Button type="submit" variant="accent" className="manual__go" {...busy.buttonProps} statusLabels={{ loading: tr('正在搜索'), success: tr('已找到设备'), error: tr('搜索失败') }}>{tr('搜索')}</Button>
      </div>
      <p className="manual__msg" id="manual-msg" data-shown={error ? '' : undefined} aria-live="polite">
        <span>{error && <><Icon name="alert"/>{error.text}</>}</span>
      </p>
      {found && (<div className="manual__reveal" key={found.id} data-leaving={leaving ? '' : undefined} ref={(el) => { if (el)
            el.inert = leaving; }}>
          <div>
            <div className="manual__found">
              
              <ContextMenu menu={<><MenuItem icon="plus" onSelect={() => {void actions.pairFound(found).catch(()=>{});}}>{tr('配对')}</MenuItem><MenuSeparator/><MenuItem icon="block" tone="danger" onSelect={() => {void actions.block(found).catch(()=>agent.fail('persistence_failed'));}}>{tr('屏蔽')}</MenuItem></>}><NearbyRow peer={found} sub={found.deviceId} onConnect={() => actions.pairFound(found)}/></ContextMenu>
            </div>
          </div>
        </div>)}
    </form>);
}
export function PairBody({ session: s, pairing }: {
    session: PairSession;
    pairing: Pairing;
}) {
    return (<div className="pair" data-status={s.status}>
      {s.status === 'connecting' ? <div className="pair__code-area"><div className="pair__connecting"><Spinner label={tr('正在连接')}/></div><PairStatus s={s}/></div> : s.status === 'failed'
            ? <PairFailed reason={s.reason}/>
            : s.dir === 'in' ? <CodeEntry s={s} pairing={pairing}/> : <CodeShow s={s}/>}
      
      {s.status !== 'failed' && <PairNote peer={s.peer}/>}
    </div>);
}
function PairNote({ peer }: {
    peer: Peer;
}) {
    const device = <Tooltip content={<span className="pair__peer-tip">
        <span>{tr("IP 地址")}</span><span>{formatAddress(peer)}</span>
        <span>{tr("设备 ID")}</span><span>{peer.deviceId}</span>
      </span>}>
      <span className="pair__peer" tabIndex={0} aria-label={tr('此设备：IP 地址 {0}，设备 ID {1}', formatAddress(peer), peer.deviceId)}>{tr("此设备")}</span>
    </Tooltip>;
    return (<p className="pair__note">
      <Icon name="info"/>
      <span>{trNodes('完成配对后，对方可以远程操控本机，请确保信任{0}', device)}</span>
    </p>);
}
function CodeEntry({ s, pairing }: {
    s: PairSession;
    pairing: Pairing;
}) {
    const { actions } = pairing;
    const wrap = useRef<HTMLDivElement>(null);
    const busy = s.status === 'verifying' || s.status === 'success';
    const shake = useShake(s.status === 'wrong' ? s.wrongs : null);
    useEffect(() => {
        if (s.status !== 'wrong')
            return;
        const t = window.setTimeout(() => {
            actions.setInput('');
            wrap.current?.querySelector<HTMLInputElement>('.ok-pin__cell')?.focus();
        }, 650 * Math.max(speed(), 0.3));
        return () => window.clearTimeout(t);
    }, [s.status, s.wrongs]);
    useEffect(() => {
        if (s.status === 'input' && !s.input)
            wrap.current?.querySelector<HTMLInputElement>('.ok-pin__cell')?.focus();
    }, [s.status]);
    return (<div className="pair__code-area" ref={wrap}>
      <div className="pair__shake" ref={shake}>
        <PinInput length={6} type="numeric" autoFocus aria-label={tr('对方屏幕上显示的配对码')} value={s.input} onChange={actions.setInput} onComplete={actions.submit} disabled={busy} invalid={s.status === 'wrong'} className="pair__pin"/>
      </div>
      <PairStatus s={s}/>
    </div>);
}
function CodeShow({ s }: {
    s: PairSession;
}) {
    const ref = useRef<HTMLDivElement>(null);
    useEffect(() => {
        const card = ref.current?.closest('.ok-float__card');
        if (card && !card.contains(document.activeElement))
            ref.current?.focus();
        else if (document.activeElement === document.body)
            ref.current?.focus();
    }, []);
    const digits = s.code.split('');
    return (<div className="pair__code-area">
      <div className="pair__code" ref={ref} tabIndex={-1} role="group" aria-label={tr('配对码')} aria-description={s.code.split('').join(' ')}>
        {digits.map((d, i) => <span key={i} className="pair__digit">{d}</span>)}
      </div>
      <PairStatus s={s}/>
    </div>);
}
function PairStatus({ s }: {
    s: PairSession;
}) {
    let tone = 'muted';
    let body: ReactNode;
    if (s.status === 'verifying')
        body = <><Spinner size={16} variant="ring" label={tr('正在验证')}/>{tr('正在验证…')}</>;
    else if (s.status === 'success') {
        tone = 'good';
        body = <><Icon name="check-circle"/>{tr('配对成功')}</>;
    }
    else if (s.status === 'wrong') {
        tone = 'bad';
        body = <><Icon name="x-circle"/>{tr('配对码不正确，请重新输入')}</>;
    }
    else
        body = s.dir === 'in' ? null : <Dots label={tr('等待对方输入')}/>;
    return (<div className="pair__status" data-tone={tone} data-shown={body ? '' : undefined}>
      <div>
        <p className="pair__status-line" aria-live="polite">
          {body && <span key={s.status} className="pair__status-in">{body}</span>}
        </p>
      </div>
    </div>);
}
function PairFailed({ reason }: {
    reason?: string;
}) {
    return (<div className="pair__failed" role="alert">
      <span className="pair__failed-icon" aria-hidden>
        <svg viewBox="0 0 20 20"><path pathLength={1} d="M6 6l8 8"/><path pathLength={1} d="M14 6l-8 8"/></svg>
      </span>
      <strong>{tr('配对失败')}</strong>
      {reason && <span className="pair__failed-reason">{reason}</span>}
    </div>);
}
