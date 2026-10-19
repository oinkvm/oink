import { formatList, tr } from '../i18n';
import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { Button, Tooltip, speed } from 'oink-ui';
import type { PeerQuality } from '../api/types';
import { useConnectionQuality } from './useConnectionQuality';
const SPAN = 60;
const TICK = 1000;
const H = 150;
// Room for the widest two-decimal axis label, 999.99 ms.
const PAD = { l: 64, r: 8, t: 10, b: 10 };
const COLORS = ['var(--ok-info)', 'var(--ok-success)', 'var(--ok-warning)', '#C77DFF', 'var(--ok-danger)', '#14B8A6', '#FF7A2F'];
type Sample = number | null;
interface Series {
    id: string;
    name: string;
    color: string;
    values: Sample[];
    statistics: PeerQuality;
}
const niceMax = (v: number) => {
    const p = 10 ** Math.floor(Math.log10(Math.max(v, 1)));
    return [1, 2, 3, 4, 5, 6, 8, 10].map((m) => m * p).find((x) => x >= v) ?? 10 * p;
};
export function LatencyChart({ active }: {
    active: boolean;
}) {
    const { snapshot, failed } = useConnectionQuality(active);
    const colors = useRef(new Map<string, string>());
    const colorOf = (id: string) => {
        if (!colors.current.has(id)) colors.current.set(id, COLORS[colors.current.size % COLORS.length]);
        return colors.current.get(id)!;
    };
    const series: Series[] = (snapshot?.peers ?? []).map(peer => {
        const values = Array<Sample>(SPAN + 1).fill(null);
        for (const sample of peer.samples) {
            const index = SPAN - Math.round((snapshot!.sampled_at_ms - sample.at_ms) / TICK);
            if (index >= 0 && index <= SPAN) values[index] = sample.rtt_ms;
        }
        return { id: peer.device, name: peer.name, color: colorOf(peer.device), values, statistics: peer };
    });
    // Explicit selections stay local to this chart instance. While soloed,
    // newly connected devices stay hidden until the user restores all.
    const [visibleIds, setVisibleIds] = useState<Set<string> | null>(null);
    const visible = series.filter(s => visibleIds == null || visibleIds.has(s.id));
    const toggle = (id: string) => setVisibleIds(previous => {
        const next = new Set(previous ?? series.map(s => s.id));
        if (next.has(id)) next.delete(id); else next.add(id);
        return series.every(s => next.has(s.id)) ? null : next;
    });
    const isSolo = (id: string) => visible.length === 1 && visible[0].id === id;
    // Clicking the already-soloed colour dot restores all, including new peers.
    const solo = (id: string) => setVisibleIds(isSolo(id) ? null : new Set([id]));
    const empty = series.length === 0 ? tr('没有在线设备') : tr('没有显示的设备');
    const lastTick = useRef(performance.now());
    useEffect(() => { lastTick.current = performance.now(); }, [snapshot]);
    // Display values saturate at 999.99 without an overflow prefix; the
    // backend keeps original samples and statistics.
    const decimal = (value: number) => Math.min(value, 999.99).toFixed(2);
    const format = (value: number | null) => value == null ? '—' : `${decimal(value)} ms`;
    const box = useRef<HTMLDivElement>(null);
    const [w, setW] = useState(480);
    useLayoutEffect(() => {
        const el = box.current!;
        const ro = new ResizeObserver(([e]) => setW(e.contentRect.width));
        ro.observe(el);
        setW(el.getBoundingClientRect().width || 480);
        return () => ro.disconnect();
    }, []);
    const plotW = Math.max(40, w - PAD.l - PAD.r);
    const step = plotW / SPAN;
    const lines = useRef<SVGGElement>(null);
    useEffect(() => {
        if (!active)
            return;
        let raf = 0;
        const frame = () => {
            const s = speed();
            const f = s > 0 ? Math.min(1, (performance.now() - lastTick.current) / TICK) : 0;
            lines.current?.setAttribute('transform', `translate(${(-f * step).toFixed(2)} 0)`);
            raf = requestAnimationFrame(frame);
        };
        raf = requestAnimationFrame(frame);
        return () => cancelAnimationFrame(raf);
    }, [active, step]);
    const max = Math.min(999.99, niceMax(Math.max(10, ...visible.flatMap((s) => s.values.filter((v): v is number => v != null))) * 1.1));
    const y = (v: number) => PAD.t + (1 - Math.min(v, 999.99) / max) * (H - PAD.t - PAD.b);
    const x = (i: number) => PAD.l + (i + 1) * step;
    const pathOf = (vals: Sample[]) => {
        let d = '';
        let pen = false;
        vals.forEach((v, i) => {
            if (v == null) {
                pen = false;
                return;
            }
            d += `${pen ? 'L' : 'M'}${x(i).toFixed(1)} ${y(v).toFixed(1)}`;
            pen = true;
        });
        return d;
    };
    const [hover, setHover] = useState<number | null>(null);
    const onMove = (e: React.PointerEvent) => {
        const r = e.currentTarget.getBoundingClientRect();
        const px = e.clientX - r.left;
        if (px < PAD.l || px > w - PAD.r) {
            setHover(null);
            return;
        }
        setHover(Math.min(SPAN, Math.max(0, Math.round((px - PAD.l) / step - 0.5))));
    };
    const hoverRows = hover != null ? visible.map((s) => ({ ...s, v: s.values[hover] })).filter((s) => s.v != null) : [];
    const ago = hover != null ? SPAN - hover : 0;
    const current = visible.map(s => s.statistics.current_ms == null ? tr('{0}：延迟未知', s.name) : tr('{0} {1} 毫秒', s.name, decimal(s.statistics.current_ms)));
    return (<div className="lat">
      <div className="lat__plot" ref={box} onPointerMove={onMove} onPointerLeave={() => setHover(null)}>
        <svg width={w} height={H} role="img" aria-label={tr('设备延迟：{0}', formatList(current) || empty)}>
          <defs>
            <clipPath id="lat-clip"><rect x={PAD.l} y={0} width={plotW} height={H}/></clipPath>
          </defs>
          {[0, 0.5, 1].map((k) => (<g key={k} className="lat__grid">
              <line x1={PAD.l} x2={w - PAD.r} y1={y(max * k)} y2={y(max * k)}/>
              <text x={PAD.l - 8} y={y(max * k)} dy="0.32em" textAnchor="end">{format(max * k)}</text>
            </g>))}
          <g clipPath="url(#lat-clip)">
            <g ref={lines}>
              {visible.map((s) => <path key={s.id} className="lat__line" d={pathOf(s.values)} stroke={s.color}/>)}
              
              {hover != null && (<g className="lat__cursor">
                  <line x1={x(hover)} x2={x(hover)} y1={PAD.t} y2={H - PAD.b}/>
                  {hoverRows.map((s) => <circle key={s.id} cx={x(hover)} cy={y(s.v!)} r="3.2" fill={s.color}/>)}
                </g>)}
            </g>
          </g>
        </svg>
        {hover != null && hoverRows.length > 0 && (<div className="lat__tip" style={{ left: Math.max(PAD.l, Math.min(x(hover) - step + 10, w - 170)) }} aria-hidden>
            <div className="lat__tip-t">{ago === 0 ? tr('刚刚') : tr('{0} 秒前', ago)}</div>
            {hoverRows.map((s) => (<div key={s.id} className="lat__tip-row"><i style={{ background: s.color }}/><span>{s.name}</span><b>{format(s.v!)}</b></div>))}
          </div>)}
        {visible.length === 0 && <p className="lat__empty">{empty}</p>}
      </div>
      {failed && <p role="status">{tr('无法读取连接质量')}</p>}
      <div className="lat__table-scroll">
        <table className="lat__table" aria-label={tr('连接质量')}>
          <thead><tr>
            <th scope="col">{tr('设备名称')}</th><th scope="col">{tr('当前延迟')}</th>
            <th scope="col">P50</th><th scope="col">P99</th>
            <th scope="col">{tr('抖动（平均）')}</th><th scope="col">{tr('路径')}</th>
          </tr></thead>
          <tbody>{series.map(s => <tr key={s.id}>
            <td><span className="lat__device">
              <Tooltip interactive={false} content={isSolo(s.id) ? tr('显示所有设备的曲线') : tr('只显示 {0} 的曲线', s.name)}>
                <Button variant="ghost" size="sm" iconOnly className="lat__dot" data-series-hidden={visibleIds != null && !visibleIds.has(s.id) || undefined} aria-pressed={isSolo(s.id)} aria-label={isSolo(s.id) ? tr('显示所有设备的曲线') : tr('只显示 {0} 的曲线', s.name)} onClick={() => solo(s.id)}>
                  <i aria-hidden style={{background:s.color}}/>
                </Button>
              </Tooltip>
              <button type="button" className="lat__name" aria-pressed={visibleIds == null || visibleIds.has(s.id)} aria-label={visibleIds == null || visibleIds.has(s.id) ? tr('隐藏 {0} 的曲线', s.name) : tr('显示 {0} 的曲线', s.name)} onClick={() => toggle(s.id)}>{s.name}</button>
            </span></td>
            {[s.statistics.current_ms,s.statistics.p50_ms,s.statistics.p99_ms,s.statistics.average_jitter_ms].map((value,index) => <td key={index}>{format(value)}</td>)}
            <td>{s.statistics.path.type === 'relay' ? tr('经 {0} 中转', s.statistics.path.via_name) : tr('直连')}</td>
          </tr>)}</tbody>
        </table>
      </div>
    </div>);
}
