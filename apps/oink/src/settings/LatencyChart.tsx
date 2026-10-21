import { formatList, tr, useLanguage } from '../i18n';
import { memo, useId, useLayoutEffect, useRef, useState } from 'react';
import { Button, Tooltip } from 'oink-ui';
import type { PeerQuality, AudioSourceQuality } from '../api/types';
import { useAudioQuality } from './useAudioQuality';
import { deviceColor } from './deviceColors';
import { useConnectionQuality } from './useConnectionQuality';
import { useVisible } from '../window/useVisible';
const SPAN = 60;
const H = 150;
// Room for the widest two-decimal axis label, 999.99 ms.
const PAD = { l: 64, r: 8, t: 10, b: 10 };
type Sample = number | null;
interface Series {
    id: string;
    name: string;
    color: string;
    values: Sample[];
    statistics: PeerQuality | AudioSourceQuality;
}
const niceMax = (v: number) => {
    const p = 10 ** Math.floor(Math.log10(Math.max(v, 1)));
    return [1, 2, 3, 4, 5, 6, 8, 10].map((m) => m * p).find((x) => x >= v) ?? 10 * p;
};
export const LatencyChart = memo(function LatencyChart({active}: {active:boolean}) {
    return <DiagnosticsChart active={active}/>;
});
export const DiagnosticsChart = memo(function DiagnosticsChart({ active, audio = false }: {
    active: boolean; audio?: boolean;
}) {
    useLanguage();
    const root = useRef<HTMLDivElement>(null);
    const visibleView = useVisible(root, active);
    const connection = useConnectionQuality(visibleView && !audio);
    const quality = useAudioQuality(visibleView && audio);
    const snapshot = audio ? quality.snapshot : connection.snapshot;
    const failed = audio ? quality.failed : connection.failed;
    const clipId = useId();
    const tick = audio ? 250 : 1000;
    const slots = SPAN * 1000 / tick;
    const peers = audio ? quality.snapshot?.sources ?? [] : connection.snapshot?.peers ?? [];
    const series: Series[] = peers.map(peer => {
        const values = Array<Sample>(slots + 1).fill(null);
        for (const sample of peer.samples) {
            const index = slots - Math.round((snapshot!.sampled_at_ms - sample.at_ms) / tick);
            if (index >= 0 && index <= slots) values[index] = 'buffer_ms' in sample ? sample.buffer_ms : sample.rtt_ms;
        }
        return { id: peer.device, name: peer.name, color: deviceColor(peer.device), values, statistics: peer };
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
    const empty = series.length === 0 ? (audio ? tr('尚未接收音频') : tr('没有在线设备')) : tr('没有显示的设备');
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
    const step = plotW / slots;
    // Render the retained source tick directly. A display-rate translation would
    // repaint identical telemetry at the monitor's refresh rate.
    const max = Math.min(999.99, niceMax(Math.max(10, ...visible.flatMap((s) => s.values.filter((v): v is number => v != null))) * 1.1));
    const y = (v: number) => PAD.t + (1 - Math.min(v, 999.99) / max) * (H - PAD.t - PAD.b);
    const x = (i: number) => PAD.l + i * step;
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
    // Mark every affected source, including a series hidden by its legend.
    const glitchSeconds = new Map<number, Series[]>();
    if (audio) for (const source of series) {
        if (!('glitches' in source.statistics)) continue;
        for (const bucket of source.statistics.glitches) {
            if (bucket.lost + bucket.late === 0) continue;
            const second = Math.floor(bucket.at_ms / 1000) * 1000;
            if (snapshot!.sampled_at_ms / 1000 - second / 1000 >= 60 || second > snapshot!.sampled_at_ms) continue;
            const devices = glitchSeconds.get(second) ?? [];
            if (!devices.some(s => s.id === source.id)) devices.push(source);
            glitchSeconds.set(second, devices);
        }
    }
    const glitchX = (second: number) => PAD.l + (1 - (snapshot!.sampled_at_ms - second) / 60000) * plotW;
    const [glitchHover, setGlitchHover] = useState<number | null>(null);
    const [hover, setHover] = useState<number | null>(null);
    const onMove = (e: React.PointerEvent) => {
        const r = e.currentTarget.getBoundingClientRect();
        const px = e.clientX - r.left;
        const target = e.target instanceof Element ? e.target.closest<HTMLElement>('[data-glitch-second]') : null;
        const near = target ? Number(target.dataset.glitchSecond)
            : [...glitchSeconds.keys()].find(second => Math.abs(glitchX(second) - px) <= 6) ?? null;
        setGlitchHover(near);
        if (px < PAD.l || px > w - PAD.r) {
            setHover(null);
            return;
        }
        setHover(Math.min(slots, Math.max(0, Math.round((px - PAD.l) / step))));
    };
    const hoverRows = hover != null ? visible.map((s) => ({ ...s, v: s.values[hover] })).filter((s) => s.v != null) : [];
    const ago = hover != null ? (slots - hover) * tick / 1000 : 0;
    const current = visible.map(s => {
        const value = audio ? s.values.filter(v => v != null).at(-1)
            : 'current_ms' in s.statistics ? s.statistics.current_ms : null;
        return value == null ? tr('{0}：延迟未知', s.name) : tr('{0} {1} 毫秒', s.name, decimal(value));
    });
    return (<div className="lat" ref={root}>
      <div className="lat__plot" ref={box} onPointerMove={onMove} onPointerLeave={() => {setHover(null);setGlitchHover(null);}}>
        <svg width={w} height={H} role="img" aria-label={audio ? tr('音频缓冲：{0}', formatList(current) || empty) : tr('设备延迟：{0}', formatList(current) || empty)}>
          <defs>
            <clipPath id={clipId}><rect x={PAD.l} y={0} width={plotW} height={H}/></clipPath>
          </defs>
          {[0, 0.5, 1].map((k) => (<g key={k} className="lat__grid">
              <line x1={PAD.l} x2={w - PAD.r} y1={y(max * k)} y2={y(max * k)}/>
              <text x={PAD.l - 8} y={y(max * k)} dy="0.32em" textAnchor="end">{format(max * k)}</text>
            </g>))}
          <g clipPath={`url(#${clipId})`}>
            <g>
              {visible.map((s) => <path key={s.id} className="lat__line" data-device={s.id} d={pathOf(s.values)} stroke={s.color}/>)}
              
              {[...glitchSeconds.keys()].map(second => <line key={second} className="lat__glitch" x1={glitchX(second)} x2={glitchX(second)} y1={PAD.t} y2={H - PAD.b}/>)}
              {hover != null && glitchHover == null && (<g className="lat__cursor">
                  <line x1={x(hover)} x2={x(hover)} y1={PAD.t} y2={H - PAD.b}/>
                  {hoverRows.map((s) => <circle key={s.id} cx={x(hover)} cy={y(s.v!)} r="3.2" fill={s.color}/>)}
                </g>)}
            </g>
          </g>
        </svg>
        {[...glitchSeconds].map(([second, devices]) => <Tooltip key={second} interactive={false} open={glitchHover === second} delay={0} placement="right" content={<div className="lat__glitch-devices">{devices.map(s => <div key={s.id} className="lat__tip-row"><i aria-hidden style={{background:s.color}}/><span>{s.name}</span></div>)}</div>}>
          <span className="lat__glitch-hit" data-glitch-second={second} style={{left:glitchX(second)-Math.min(6,plotW/120),width:Math.min(12,plotW/60),top:PAD.t,height:H-PAD.t-PAD.b}}/>
        </Tooltip>)}
        {hover != null && glitchHover == null && hoverRows.length > 0 && (<div className="lat__tip" style={{ left: Math.max(PAD.l, Math.min(x(hover) + 10, w - 170)) }} aria-hidden>
            <div className="lat__tip-t">{ago === 0 ? tr('刚刚') : tr('{0} 秒前', ago)}</div>
            {hoverRows.map((s) => (<div key={s.id} className="lat__tip-row"><i style={{ background: s.color }}/><span>{s.name}</span><b>{format(s.v!)}</b></div>))}
          </div>)}
        {visible.length === 0 && <p className="lat__empty">{empty}</p>}
      </div>
      {failed && <p role="status">{audio ? tr('无法读取音频质量') : tr('无法读取连接质量')}</p>}
      {series.length > 0 && <div className="lat__table-scroll">
        <table className={audio ? "lat__table lat__table--audio" : "lat__table"} aria-label={audio ? tr('音频质量') : tr('连接质量')}>
          <thead><tr>
            <th scope="col">{tr('设备名称')}</th>
            {audio ? <><th scope="col">{tr('缓冲 P50')}</th><th scope="col">{tr('缓冲 P90')}</th><th scope="col">{tr('缓冲 P99')}</th><th scope="col">{tr('平均码率')}</th><th scope="col">{tr('断音')}</th></> : <><th scope="col">{tr('当前延迟')}</th><th scope="col">P50</th><th scope="col">{tr('P90')}</th><th scope="col">P99</th><th scope="col">{tr('路径')}</th></>}
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
            {'bitrate_kbps' in s.statistics ? <>
              {[s.statistics.p50_ms,s.statistics.p90_ms,s.statistics.p99_ms].map((v,i)=><td key={i}>{format(v)}</td>)}
              <td>{s.statistics.bitrate_kbps.toFixed(2)} kbps</td><td>{s.statistics.dropouts}</td>
            </> : <>
              {[s.statistics.current_ms,s.statistics.p50_ms,s.statistics.p90_ms,s.statistics.p99_ms].map((value,index) => <td key={index}>{format(value)}</td>)}
              <td>{s.statistics.path.type === 'relay' ? tr('经 {0} 中转', s.statistics.path.via_name) : tr('直连')}</td>
            </>}

          </tr>)}</tbody>
        </table>
      </div>}
    </div>);
});
