import { formatList, tr, useLanguage } from '../i18n';
import { memo, useId, useLayoutEffect, useRef, useState, useSyncExternalStore } from 'react';
import { Button, Segmented, Tooltip } from 'oink-ui';
import type { PeerQuality, AudioSourceQuality } from '../api/types';
import { useAudioQuality } from './useAudioQuality';
import { deviceColor } from './deviceColors';
import { useConnectionQuality } from './useConnectionQuality';
import { useVisible } from '../window/useVisible';
/** Chart window in seconds. The backend retains the longer one. */
export type ChartSpan = 60 | 300;
export const DEFAULT_SPAN: ChartSpan = 300;
// Session-only UI state survives Settings unmounts; never written to preferences.
const spans: Record<'latency' | 'audioQuality', ChartSpan> = { latency: DEFAULT_SPAN, audioQuality: DEFAULT_SPAN };
const spanListeners = new Set<() => void>();
const subscribeSpan = (listener: () => void) => { spanListeners.add(listener); return () => { spanListeners.delete(listener); }; };
export function useChartSpan(id: 'latency' | 'audioQuality'): [ChartSpan, (value: ChartSpan) => void] {
    const span = useSyncExternalStore(subscribeSpan, () => spans[id]);
    return [span, value => { spans[id] = value; spanListeners.forEach(listener => listener()); }];
}
// The 5-minute view plots the maximum of each absolute 5-second bucket so a
// spike stays visible at the coarser scale; the 1-minute view stays raw.
const BUCKET_MS = 5000;
const H = 150;
// Room for the widest two-decimal axis label, 999.99 ms, and the time axis.
const PAD = { l: 64, r: 8, t: 10, b: 30 };
const AXIS = { 60: [60, 45, 30, 15, 0], 300: [300, 240, 180, 120, 60, 0] } as const;
const axisLabel = (span: ChartSpan, seconds: number) => seconds === 0 ? tr('现在') : span === 300 ? tr('{0} 分钟前', seconds / 60) : tr('{0} 秒前', seconds);
const agoLabel = (ms: number) => {
    const seconds = Math.round(ms / 1000);
    if (seconds <= 0) return tr('刚刚');
    return seconds <= 60 ? tr('{0} 秒前', seconds) : tr('{0} 分 {1} 秒前', Math.floor(seconds / 60), seconds % 60);
};
export const spanDescription = (id: 'latency' | 'audioQuality', span: ChartSpan) => id === 'latency'
    ? (span === 300 ? tr('最近 5 分钟每台在线设备的延迟') : tr('最近 1 分钟每台在线设备的延迟'))
    : (span === 300 ? tr('最近 5 分钟各来源的缓冲、播放断续、补音和迟到包') : tr('最近 1 分钟各来源的缓冲、播放断续、补音和迟到包'));
export function ChartSpanToggle({ value, onChange, label }: { value: ChartSpan; onChange: (span: ChartSpan) => void; label: string }) {
    useLanguage();
    return <Segmented<'60' | '300'> size="sm" className="lat__span" aria-label={tr('{0}的时间范围', label)} value={String(value) as '60' | '300'}
        onChange={v => onChange(Number(v) as ChartSpan)} options={[{ value: '60', label: tr('1 分钟') }, { value: '300', label: tr('5 分钟') }]}/>;
}
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
export const LatencyChart = memo(function LatencyChart({active, span = DEFAULT_SPAN}: {active:boolean; span?: ChartSpan}) {
    return <DiagnosticsChart active={active} span={span}/>;
});
export const DiagnosticsChart = memo(function DiagnosticsChart({ active, audio = false, span = DEFAULT_SPAN }: {
    active: boolean; audio?: boolean; span?: ChartSpan;
}) {
    useLanguage();
    const root = useRef<HTMLDivElement>(null);
    const visibleView = useVisible(root, active);
    const connection = useConnectionQuality(visibleView && !audio);
    const quality = useAudioQuality(visibleView && audio);
    const snapshot = audio ? quality.snapshot : connection.snapshot;
    const failed = audio ? quality.failed : connection.failed;
    const clipId = useId();
    const spanMs = span * 1000;
    const bucketed = span === 300;
    const tick = bucketed ? BUCKET_MS : audio ? 250 : 1000;
    const slots = spanMs / tick;
    const now = snapshot?.sampled_at_ms ?? 0;
    // Raw slots sit at their sample's age. Buckets are aligned to absolute
    // time so a max does not move between buckets as the window advances;
    // each bucket is drawn, hovered and labelled at its start.
    const lastBucket = Math.floor(now / BUCKET_MS);
    const slotStart = (i: number) => bucketed ? (lastBucket - (slots - i)) * BUCKET_MS : now - (slots - i) * tick;
    const slotAge = (i: number) => now - slotStart(i);
    const inWindow = (at: number) => at <= now && now - at < spanMs;
    const peers = audio ? quality.snapshot?.sources ?? [] : connection.snapshot?.peers ?? [];
    const series: Series[] = peers.map(peer => {
        const values = Array<Sample>(slots + 1).fill(null);
        for (const sample of peer.samples) {
            const value = 'buffer_ms' in sample ? sample.buffer_ms : sample.rtt_ms;
            const index = bucketed ? slots - (lastBucket - Math.floor(sample.at_ms / BUCKET_MS))
                : slots - Math.round((now - sample.at_ms) / tick);
            if (!inWindow(sample.at_ms) || index < 0 || index > slots || value == null) continue;
            if (bucketed && slotAge(index) >= spanMs) continue;
            values[index] = bucketed ? Math.max(values[index] ?? value, value) : value;
        }
        const kind='kind' in peer?peer.kind:undefined;
        const id=kind?`${peer.device}:${kind}`:peer.device;
        // The same device can supply both streams; keep their histories and
        // every label (legend, tooltip and accessibility text) distinguishable.
        const name=kind === 'microphone' ? tr('{0} · 麦克风', peer.name)
            : kind === 'speaker' ? tr('{0} · 扬声器', peer.name) : peer.name;
        const statistics = span === 300 ? { ...peer, ...peer.stats_5m } : peer;
        return { id, name, color: deviceColor(kind==='microphone'?id:peer.device), values, statistics };
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
    // Render the retained source tick directly. A display-rate translation would
    // repaint identical telemetry at the monitor's refresh rate.
    const max = Math.min(999.99, niceMax(Math.max(10, ...visible.flatMap((s) => s.values.filter((v): v is number => v != null))) * 1.1));
    const y = (v: number) => PAD.t + (1 - Math.min(v, 999.99) / max) * (H - PAD.t - PAD.b);
    const xAge = (age: number) => PAD.l + (1 - age / spanMs) * plotW;
    const x = (i: number) => xAge(slotAge(i));
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
    const rawOutput = audio ? quality.snapshot?.output : null;
    const output = rawOutput && (span === 300 ? { ...rawOutput, ...rawOutput.stats_5m } : rawOutput);
    // Markers use the same slot as the line: the second in the raw view, the
    // 5-second bucket start in the 5-minute view.
    const markerOf = (at: number) => bucketed ? Math.floor(at / BUCKET_MS) * BUCKET_MS : Math.floor(at / 1000) * 1000;
    const glitchSeconds = new Map<number, Pick<Series, 'id' | 'name' | 'color'>[]>();
    if (audio) for (const source of series) {
        if (!('glitches' in source.statistics)) continue;
        for (const bucket of source.statistics.glitches) {
            if (bucket.interruptions === 0 && (bucket.discontinuities ?? 0) === 0) continue;
            if (!inWindow(bucket.at_ms)) continue;
            const second = markerOf(bucket.at_ms);
            if (!inWindow(second)) continue;
            const devices = glitchSeconds.get(second) ?? [];
            if (!devices.some(s => s.id === source.id)) devices.push(source);
            glitchSeconds.set(second, devices);
        }
    }
    if (output) for (const bucket of output.glitches) {
        if (bucket.interruptions === 0 && (bucket.discontinuities ?? 0) === 0) continue;
        if (!inWindow(bucket.at_ms)) continue;
        const second = markerOf(bucket.at_ms);
        if (!inWindow(second)) continue;
        const devices = glitchSeconds.get(second) ?? [];
        if (!devices.some(s => s.id === 'output')) devices.push({id:'output', name:tr('本机处理'), color:'var(--ok-danger)'});
        glitchSeconds.set(second, devices);
    }
    const glitchX = (second: number) => xAge(now - second);
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
        let nearest = 0;
        for (let i = 1; i <= slots; i++) if (Math.abs(x(i) - px) < Math.abs(x(nearest) - px)) nearest = i;
        setHover(nearest);
    };
    const hoverRows = hover != null ? visible.map((s) => ({ ...s, v: s.values[hover] })).filter((s) => s.v != null) : [];
    const ago = hover != null ? Math.max(0, slotAge(hover)) : 0;
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
          <g className="lat__axis">{AXIS[span].map((seconds, index, all) => <g key={seconds}>
              <line x1={xAge(seconds * 1000)} x2={xAge(seconds * 1000)} y1={H - PAD.b} y2={H - PAD.b + 4}/>
              <text x={xAge(seconds * 1000)} y={H - PAD.b + 17} textAnchor={index === 0 ? 'start' : index === all.length - 1 ? 'end' : 'middle'}>{axisLabel(span, seconds)}</text>
            </g>)}</g>
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
            <div className="lat__tip-t">{agoLabel(ago)}</div>
            {hoverRows.map((s) => (<div key={s.id} className="lat__tip-row"><i style={{ background: s.color }}/><span>{s.name}</span><b>{format(s.v!)}</b></div>))}
          </div>)}
        {visible.length === 0 && <p className="lat__empty">{empty}</p>}
      </div>
      {failed && <p role="status">{audio ? tr('无法读取音频质量') : tr('无法读取连接质量')}</p>}
      {(series.length > 0 || output) && <div className="lat__table-scroll ok-scroll">
        <table className={audio ? "lat__table lat__table--audio" : "lat__table"} aria-label={audio ? tr('音频质量') : tr('连接质量')}>
          <thead><tr>
            <th scope="col">{tr('设备名称')}</th>
            {audio ? <><th scope="col">{tr('平均缓冲')}</th><th scope="col">{tr('平均码率')}</th><th scope="col">{tr('中断')}</th><th scope="col">{tr('补音时长')}</th><th scope="col">{tr('迟到包')}</th><th scope="col">{tr('不连续')}</th></> : <><th scope="col">{tr('当前延迟')}</th><th scope="col">P50</th><th scope="col">{tr('P90')}</th><th scope="col">P99</th><th scope="col">{tr('路径')}</th></>}
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
              <td>{format(s.statistics.buffer_avg_ms)}</td>
              <td>{s.statistics.bitrate_kbps.toFixed(2)} kbps</td><td>{(s.statistics.output_interruptions ?? 0) > 0
                ? <Tooltip interactive={false} content={tr('其中 {0} 次发生在本机输出，影响了此来源，不代表发送端故障。', s.statistics.output_interruptions)}><span>{s.statistics.interruptions}</span></Tooltip>
                : s.statistics.interruptions}</td>
              <td>{s.statistics.concealed_ms.toFixed(2)} ms</td><td>{s.statistics.late_packets}</td>
              <td>{s.statistics.discontinuities ?? 0}</td>
            </> : <>
              {[s.statistics.current_ms,s.statistics.p50_ms,s.statistics.p90_ms,s.statistics.p99_ms].map((value,index) => <td key={index}>{format(value)}</td>)}
              <td>{s.statistics.path.type === 'relay' ? tr('经 {0} 中转', s.statistics.path.via_name) : tr('直连')}</td>
            </>}

          </tr>)}{output && <tr>
            <td><Tooltip interactive={false} content={tr('本机输出缺音 {0} 毫秒；回调超时 {1} 次，最长处理 {2} 毫秒，最小余量 {3} 毫秒。超时本身不代表缺音；来源丢包补偿仍计入来源。', output.missing_ms.toFixed(2), output.callback_over_budget ?? '—', output.callback_max_work_us == null ? '—' : (output.callback_max_work_us / 1000).toFixed(2), output.callback_min_margin_us == null ? '—' : (output.callback_min_margin_us / 1000).toFixed(2)) + ' ' + tr('原生时钟覆盖 {0}/{1} 次回调，确认缺口 {2} 帧。', output.native_timing_callbacks ?? 0, output.callbacks ?? 0, (output.native_timing_callbacks ?? 0) > 0 ? output.native_gap_frames ?? 0 : '—')}><span>{tr('本机处理')}</span></Tooltip></td>
            <td>—</td><td>—</td><td>{output.interruptions}</td><td>—</td><td>—</td><td>{output.discontinuities ?? 0}</td>
          </tr>}</tbody>
        </table>
      </div>}
    </div>);
});
