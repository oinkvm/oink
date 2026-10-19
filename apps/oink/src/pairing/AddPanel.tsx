import { tr } from '../i18n';
import { useEffect, useId, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { usePeekScope } from '../devices/peek';
import type { PeekHandlers } from '../devices/peek';
import { Button, Dialog, Dots, Icon, Tooltip, VisuallyHidden, speed, useBusy } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { OsLogo } from '../devices/icons';
import { formatAddress } from './data';
import { agent, useAgent } from '../api/store';
import type { IncomingRequest, Peer } from './data';
import type { Pairing } from './usePairing';
import { useListPresence } from './useListPresence';
import type { ItemPhase } from './useListPresence';
import './pairing.css';
export function AddPanel({ pairing }: {
    pairing: Pairing;
}) {
    const { nearby, actions } = pairing;
    const near = useListPresence(nearby);
    return (<div className="addp">
      <section className="addp__group" aria-labelledby="addp-near">
        <h3 className="devs__label addp__label" id="addp-near">
          <span>{tr('附近设备')}</span><span className="devs__count">{nearby.length || ''}</span>
          <span className="addp__tools">
            <Tooltip interactive={false} content={tr('输入 IP 配对')} placement="top">
              <Button variant="ghost" size="sm" iconOnly className="addp__tool" aria-label={tr('输入 IP 配对')} onClick={actions.openManual}>
                <Icon name="plus"/>
              </Button>
            </Tooltip>
            <RefreshButton onRefresh={actions.refresh}/>
          </span>
        </h3>
        <ul className="addp__list">
          {near.map(({ item, phase }) => (<FoldItem key={item.id} phase={phase}>
              <NearbyRow peer={item} onConnect={() => actions.connect(item)} ctx/>
            </FoldItem>))}
        </ul>
        <p className="addp__empty"><Dots label={tr('正在搜索')}/>{tr('正在搜索附近的设备')}</p>
      </section>
    </div>);
}
export function RefreshButton({ onRefresh }: {
    onRefresh: () => Promise<void>;
}) {
    const [loading, setLoading] = useState(false);
    const icon = useRef<HTMLSpanElement>(null);
    const spin = useRef<Animation | undefined>(undefined);
    useEffect(() => () => spin.current?.cancel(), []);
    const run = () => {
        if (loading)
            return;
        setLoading(true);
        const s = speed();
        spin.current?.cancel();
        spin.current = s > 0 ? icon.current?.animate({ transform: ['rotate(0deg)', 'rotate(360deg)'] }, { duration: 720 * s, iterations: Infinity }) : undefined;
        onRefresh().catch(()=>agent.fail('unavailable')).finally(() => {
            setLoading(false);
            const a = spin.current;
            const it = a?.effect?.getComputedTiming().currentIteration;
            if (a && it != null)
                a.effect?.updateTiming({ iterations: it + 1 });
        });
    };
    return (<Tooltip interactive={false} content={tr('刷新')} placement="top">
      <Button variant="ghost" size="sm" iconOnly className="addp__tool" aria-label={tr('刷新附近设备')} data-loading={loading ? '' : undefined} onClick={run}>
        <span className="addp__spin" ref={icon}><Icon name="refresh"/></span>
        <VisuallyHidden role="status" aria-live="polite">{loading ? tr('正在刷新') : ''}</VisuallyHidden>
      </Button>
    </Tooltip>);
}
export function AddHead({ open, title, onBack }: {
    open: boolean;
    title: string;
    onBack: () => void;
}) {
    const last = useRef(title);
    if (open)
        last.current = title;
    return (<div className="addp__head-fold" data-open={open ? '' : undefined} aria-hidden={!open || undefined} ref={(el) => { if (el)
        el.inert = !open; }}>
      <div>
        <div className="addp__head">
          <Tooltip interactive={false} content={tr('返回设备列表')} placement="bottom">
            <Button variant="ghost" size="sm" iconOnly aria-label={tr('返回设备列表')} onClick={onBack}>
              <Icon name="arrow-left"/>
            </Button>
          </Tooltip>
          <h2 className="addp__title">{last.current}</h2>
        </div>
      </div>
    </div>);
}
export function RequestsGroup({ pairing, collapsed, onExpand, hidden = false }: {
    pairing: Pairing;
    hidden?: boolean;
    collapsed: boolean;
    onExpand: () => void;
}) {
    const { requests, actions } = pairing;
    const reqs = useListPresence(requests);
    const open = !hidden && requests.length > 0;
    const headingId = useId();
    const peek = usePeekScope();
    return (<section className="addp__group addp__fold" data-open={open ? '' : undefined} data-collapsed={collapsed ? '' : undefined} aria-labelledby={headingId} aria-hidden={!open || undefined}>
      <div className="addp__fold-in">
        <h3 className="devs__label addp__label" id={headingId}>
          <span>{tr('配对请求')}</span><span className="devs__count">{requests.length || ''}</span>
        </h3>
        <ul className="addp__list">
          {reqs.map(({ item, phase }) => (<FoldItem key={item.id} phase={phase}>
              <RequestRow req={item} collapsed={collapsed} onExpand={onExpand} onAccept={() => actions.accept(item)} onReject={() => actions.reject(item)} peek={collapsed && peek ? peek.handlers(requestPeekId(item.id)) : undefined}/>
            </FoldItem>))}
        </ul>
      </div>
    </section>);
}
const SELF_IP_LIMIT = 2;
export function SelfInfo({ container }: {
    container: HTMLElement | null;
}) {
    const [all, setAll] = useState(false);
    const { network, overview } = useAgent();
    const SELF = { ips: network?.addresses.map(a => a.ip) ?? [], deviceId: overview?.this_device.fingerprint ?? '—' };
    const shown = SELF.ips.slice(0, SELF_IP_LIMIT);
    const more = SELF.ips.length > SELF_IP_LIMIT;
    const containedProps = { contained: true } as Partial<DialogProps>;
    return (<div className="self-info" role="group" aria-label={tr('本机信息')}>
      <span className="self-info__label">{tr('本机 IP')}</span>
      <span className="self-info__values">
        {shown.map((ip) => <span key={ip} className="self-info__value" title={ip}>{ip}</span>)}
        {more && (<button type="button" className="self-info__more ok-focusable" aria-haspopup="dialog" onClick={() => setAll(true)}>{tr('查看全部{0}个', SELF.ips.length)}<Icon name="chevron-right"/>
          </button>)}
      </span>
      <span className="self-info__label">{tr('本机 ID')}</span>
      <span className="self-info__values"><span className="self-info__value">{SELF.deviceId}</span></span>
      {more && (<Dialog {...containedProps} open={all} onOpenChange={setAll} container={container} size={360} title={tr('本机 IP 地址')} actions={<Button onClick={() => setAll(false)}>{tr('关闭')}</Button>}>
          <ul className="self-ips">
            {SELF.ips.map((ip) => (<li key={ip}>
                <span className="self-ips__ip">{ip}</span>
              </li>))}
          </ul>
        </Dialog>)}
    </div>);
}
export function FoldItem({ phase, children }: {
    phase: ItemPhase;
    children: ReactNode;
}) {
    return (<li className="addp__item" data-phase={phase} aria-hidden={phase === 'leave' || undefined} ref={(el) => { if (el)
        el.inert = phase === 'leave'; }}>
      <div className="addp__item-in">{children}</div>
    </li>);
}
export function PeerInfo({ peer, sub, mark }: {
    peer: Peer;
    sub: ReactNode;
    mark?: ReactNode;
}) {
    return (<>
      <span className="dev__os"><OsLogo os={peer.os}/>{mark}</span>
      <span className="dev__text">
        <span className="dev__name">{peer.name}</span>
        <span className="peer__sub">{sub}</span>
      </span>
    </>);
}
function RequestMark({ shown }: {
    shown: boolean;
}) {
    return (<span className="req-mark" data-shown={shown ? '' : undefined} aria-hidden>
      
      <svg className="req-mark__dot" data-kind="reject" viewBox="-1.5 -1.5 15 15"><circle cx="6" cy="6" r="6"/><path d="M4.2 4.2l3.6 3.6M7.8 4.2 4.2 7.8"/></svg>
      <svg className="req-mark__dot" data-kind="accept" viewBox="-1.5 -1.5 15 15"><circle cx="6" cy="6" r="6"/><path d="M3.7 6.2 5.3 7.8 8.4 4.5"/></svg>
    </span>);
}
export const requestPeekId = (id: string) => `request:${id}`;
export const fromRequestPeekId = (id: string) => (id.startsWith('request:') ? id.slice(8) : null);
export function RequestRow({ req, collapsed, onExpand, onAccept, onReject, peek, inPeek }: {
    req: IncomingRequest;
    collapsed: boolean;
    onExpand: () => void;
    onAccept: () => void;
    onReject: () => void;
    peek?: PeekHandlers;
    inPeek?: boolean;
}) {
    return (<div className="peer" data-ctx={`request:${req.id}`}>
      <PeerInfo peer={req} sub={req.ip} mark={<RequestMark shown={collapsed}/>}/>
      
      {collapsed && (<button type="button" className="peer__expand ok-focusable" aria-label={tr('{0}（{1}）请求配对，展开侧栏处理', req.name, req.ip)} onClick={onExpand} {...peek}/>)}
      <span className="peer__actions">
        <Button size="sm" iconOnly className="peer__btn peer__btn--reject" aria-label={tr('拒绝 {0} 的配对请求', req.name)} tabIndex={inPeek ? -1 : undefined} onClick={onReject}>
          <Icon name="x"/>
        </Button>
        <Tooltip content={req.sourceCount===null?tr('接受后，对方的整个设备组将加入当前设备组'):tr('接受后，对方设备组的约 {0} 台设备将加入当前设备组', req.sourceCount)}><Button size="sm" iconOnly className="peer__btn peer__btn--accept" aria-label={tr('接受 {0} 的配对请求', req.name)} tabIndex={inPeek ? -1 : undefined} onClick={onAccept}>
          <Icon name="check"/>
        </Button></Tooltip>
      </span>
    </div>);
}
const nearbyConnectors = new Map<string, () => void>();
export const connectNearby = (id: string) => nearbyConnectors.get(id)?.();
export function NearbyRow({ peer, onConnect, sub, ctx = false }: {
    peer: Peer;
    onConnect: () => Promise<unknown> | void;
    sub?: ReactNode;
    ctx?: boolean;
}) {
    const busy = useBusy({ successHold: 600 });
    const connect = () => {
        if (busy.state === 'loading')
            return;
        const r = onConnect();
        if (r)
            busy.run(r).catch(() => { });
    };
    const connectRef = useRef(connect);
    connectRef.current = connect;
    useEffect(() => {
        if (!ctx)
            return;
        const fn = () => connectRef.current();
        nearbyConnectors.set(peer.id, fn);
        return () => { if (nearbyConnectors.get(peer.id) === fn)
            nearbyConnectors.delete(peer.id); };
    }, [ctx, peer.id]);
    return (<div className="peer" data-ctx={ctx ? `nearby:${peer.id}` : undefined}>
      <PeerInfo peer={peer} sub={!peer.compatible ? peer.problem ?? tr('协议版本不兼容') : sub ?? formatAddress(peer)}/>
      <span className="peer__actions">
        <Button size="sm" iconOnly className="peer__btn" disabled={!peer.compatible} aria-label={tr('连接 {0}', peer.name)} {...busy.buttonProps} statusLabels={{ loading: tr('正在连接 {0}', peer.name), success: tr('已连接'), error: tr('连接失败') }} onClick={connect}>
          <Icon name="plus"/>
        </Button>
      </span>
    </div>);
}
