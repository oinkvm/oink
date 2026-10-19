// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { setLanguage } from '../i18n';
import type { ConnectionQuality } from '../api/types';
import { LatencyChart } from './LatencyChart';
import { readFileSync } from 'node:fs';
const styles = readFileSync('src/settings/settings.css', 'utf8');
const mocks = vi.hoisted(() => ({ query: vi.fn(), listen: vi.fn() }));
vi.mock('../api/client', () => ({ api: { connectionQuality: mocks.query }, onAgentEvent: mocks.listen }));
const snapshot: ConnectionQuality = { revision:1,sampled_at_ms: 60000, peers: [{ device:'peer',name:'Peer',path:{type:'direct'}, samples:[{at_ms:59000,rtt_ms:4},{at_ms:60000,rtt_ms:6}], current_ms:6,p50_ms:4,p90_ms:6,p99_ms:6,max_ms:6,average_jitter_ms:1.25,max_jitter_ms:3.5 }] };
beforeEach(() => { setLanguage('en'); mocks.query.mockResolvedValue(snapshot); mocks.listen.mockResolvedValue(vi.fn()); vi.stubGlobal('requestAnimationFrame',vi.fn()); vi.stubGlobal('cancelAnimationFrame',vi.fn()); vi.stubGlobal('ResizeObserver',class {observe(){} disconnect(){}}); });
afterEach(() => { cleanup(); vi.clearAllMocks(); vi.unstubAllGlobals(); });
it('renders the backend history and four statistic columns with device colours', async () => {
  const {container} = render(<LatencyChart active/>);
  await screen.findByRole('cell',{name:'Peer'});
  expect(screen.getAllByRole('columnheader').map(x=>x.textContent)).toEqual(['Device','Current','P50','P99','Jitter (Avg.)','Path']);
  expect(screen.getAllByRole('cell').map(x=>x.textContent)).toEqual(['Peer','6.00 ms','4.00 ms','6.00 ms','1.25 ms','Direct']);
  expect(container.querySelector('.lat__dot i')?.getAttribute('style')).toContain('background');
  expect(screen.getByRole('img').getAttribute('aria-label')).not.toMatch(/60\s*(?:s|秒)/i);
  expect(container.querySelector('.lat__line')?.getAttribute('d')).toContain('L');
});
it('fetches retained history on reopen and prevents a late query overwriting live data', async () => {
  let resolve!: (value:ConnectionQuality)=>void;
  mocks.query.mockImplementationOnce(()=>new Promise<ConnectionQuality>(r=>{resolve=r;}));
  let event!: (value:unknown)=>void;
  mocks.listen.mockImplementationOnce(async (handler:(value:unknown)=>void)=>{event=handler;return vi.fn();});
  const {rerender} = render(<LatencyChart active/>);
  await waitFor(()=>expect(event).toBeDefined());
  act(()=>event({type:'connection_quality',data:{...snapshot,revision:2,sampled_at_ms:60000,peers:[{...snapshot.peers[0],current_ms:8,path:{type:'relay',via:'relay',via_name:'Relay'}}]}}));
  await act(async()=>resolve(snapshot));
  expect(screen.getByRole('cell',{name:'8.00 ms'})).toBeTruthy();
  expect(screen.getByRole('cell',{name:'Relayed via Relay'})).toBeTruthy();
  rerender(<LatencyChart active={false}/>);
  rerender(<LatencyChart active/>);
  await waitFor(()=>expect(mocks.query).toHaveBeenCalledTimes(2));
});

it('renders Chinese route and headings and preserves missing measurements', async () => {
  setLanguage('zh-Hans');
  mocks.query.mockResolvedValue({...snapshot,peers:[{...snapshot.peers[0],path:{type:'relay',via:'relay',via_name:'中转设备'},current_ms:null,average_jitter_ms:null,max_jitter_ms:null}]});
  render(<LatencyChart active/>);
  await screen.findByRole('cell',{name:'经 中转设备 中转'});
  expect(screen.getByRole('columnheader',{name:'抖动（平均）'})).toBeTruthy();
  expect(screen.getByRole('button',{name:'隐藏 Peer 的曲线'})).toBeTruthy();
  expect(screen.getByRole('button',{name:'显示所有设备的曲线'})).toBeTruthy();
  expect(screen.getAllByRole('cell',{name:'—'})).toHaveLength(2);
  expect(screen.getAllByRole('columnheader').map(x=>x.textContent)).toEqual(['设备名称','当前延迟','P50','P99','抖动（平均）','路径']);
});

const withTwoPeers: ConnectionQuality = {...snapshot,peers:[snapshot.peers[0],{...snapshot.peers[0],device:'far',name:'Far peer',samples:[{at_ms:59000,rtt_ms:100},{at_ms:60000,rtt_ms:120}],current_ms:120}]};
it('toggles names without hiding table stats, rescales axes, and retains visibility across live ticks', async () => {
  mocks.query.mockResolvedValue(withTwoPeers);
  let event!: (value:unknown)=>void;
  mocks.listen.mockImplementationOnce(async (handler:(value:unknown)=>void)=>{event=handler;return vi.fn();});
  const {container} = render(<LatencyChart active/>);
  await screen.findByRole('button',{name:'Hide Far peer in chart'});
  expect(container.querySelectorAll('.lat__line')).toHaveLength(2);
  expect(container.querySelectorAll('.lat__grid text')[2].textContent).toBe('200.00 ms');
  fireEvent.click(screen.getByRole('button',{name:'Hide Far peer in chart'}));
  expect(container.querySelectorAll('.lat__line')).toHaveLength(1);
  expect(container.querySelectorAll('.lat__grid text')[2].textContent).toBe('20.00 ms');
  expect(screen.getByRole('button',{name:'Show Far peer in chart'}).getAttribute('aria-pressed')).toBe('false');
  expect(container.querySelector('.lat__dot[data-series-hidden]')).toBeTruthy();
  expect(screen.getByRole('cell',{name:'120.00 ms'})).toBeTruthy();
  act(()=>event({type:'connection_quality',data:{...withTwoPeers,revision:2,sampled_at_ms:61000}}));
  expect(container.querySelectorAll('.lat__line')).toHaveLength(1);
  fireEvent.click(screen.getByRole('button',{name:'Hide Peer in chart'}));
  expect(container.querySelectorAll('.lat__line')).toHaveLength(0);
  expect(screen.getByText('No devices shown')).toBeTruthy();
  fireEvent.click(screen.getByRole('button',{name:'Show Far peer in chart'}));
  expect(container.querySelectorAll('.lat__line')).toHaveLength(1);
});
it('solos a device with its coloured dot, switches solo, and restores all on the same dot', async () => {
  mocks.query.mockResolvedValue(withTwoPeers);
  const {container} = render(<LatencyChart active/>);
  const solo = await screen.findByRole('button',{name:'Show only Peer in chart'});
  expect(solo.classList.contains('lat__dot')).toBe(true);
  expect(solo.querySelector('i')?.getAttribute('style')).toContain('background');
  expect(solo.querySelector('svg')).toBeNull();
  fireEvent.click(solo);
  expect(container.querySelectorAll('.lat__line')).toHaveLength(1);
  expect(screen.getByRole('button',{name:'Show all devices in chart'}).getAttribute('aria-pressed')).toBe('true');
  expect(screen.getByRole('cell',{name:'120.00 ms'})).toBeTruthy();
  fireEvent.click(screen.getByRole('button',{name:'Show only Far peer in chart'}));
  expect(screen.getByRole('button',{name:'Show Peer in chart'}).getAttribute('aria-pressed')).toBe('false');
  fireEvent.click(screen.getByRole('button',{name:'Show all devices in chart'}));
  expect(container.querySelectorAll('.lat__line')).toHaveLength(2);
  expect(container.querySelectorAll('.lat__name[aria-pressed="false"]')).toHaveLength(0);
});

it('keeps new devices hidden while soloed and includes them when restoring all', async () => {
  mocks.query.mockResolvedValue(withTwoPeers);
  let event!: (value:unknown)=>void;
  mocks.listen.mockImplementationOnce(async (handler:(value:unknown)=>void)=>{event=handler;return vi.fn();});
  const {container} = render(<LatencyChart active/>);
  fireEvent.click(await screen.findByRole('button',{name:'Show only Peer in chart'}));
  act(()=>event({type:'connection_quality',data:{...withTwoPeers,revision:2,peers:[...withTwoPeers.peers,{...snapshot.peers[0],device:'new',name:'New peer'}]}}));
  expect(container.querySelectorAll('.lat__line')).toHaveLength(1);
  expect(screen.getByRole('button',{name:'Show New peer in chart'})).toBeTruthy();
  fireEvent.click(screen.getByRole('button',{name:'Show all devices in chart'}));
  expect(container.querySelectorAll('.lat__line')).toHaveLength(3);
});

it('aligns Device with names while the focusable colour controls overhang in the reserved gutter', async () => {
  mocks.query.mockResolvedValue(withTwoPeers);
  const style = document.createElement('style'); style.textContent = styles; document.head.append(style);
  try {
    const {container} = render(<LatencyChart active/>);
    const dot = await screen.findByRole('button',{name:'Show only Peer in chart'});
    const name = screen.getByRole('button',{name:'Hide Peer in chart'});
    expect(name.querySelector('i')).toBeNull();
    expect(getComputedStyle(dot).opacity).not.toBe('0');
    expect(getComputedStyle(dot).position).toBe('absolute');
    expect(getComputedStyle(dot.parentElement!).position).toBe('relative');
    expect(getComputedStyle(dot.parentElement!).display).toBe('block');
    expect(parseFloat(getComputedStyle(dot).left)).toBeLessThan(0);
    expect(parseFloat(getComputedStyle(dot).width)).toBeGreaterThanOrEqual(28);
    expect(parseFloat(getComputedStyle(dot).height)).toBeGreaterThanOrEqual(28);
    const cell = dot.closest('td')!;
    const header = screen.getByRole('columnheader',{name:'Device'});
    expect(getComputedStyle(header).textAlign).toBe('left');
    expect(getComputedStyle(screen.getByRole('columnheader',{name:'Current'})).textAlign).toBe('right');
    expect(getComputedStyle(screen.getByRole('columnheader',{name:'Path'})).textAlign).toBe('left');
    expect(getComputedStyle(header).paddingLeft).toBe(getComputedStyle(cell).paddingLeft);
    expect(parseFloat(getComputedStyle(cell).paddingLeft)).toBeGreaterThanOrEqual(parseFloat(getComputedStyle(dot).width));
    expect(getComputedStyle(name).paddingLeft).toBe('0px');
    const cells = container.querySelectorAll('td').length;
    act(()=>dot.focus());
    expect(document.activeElement).toBe(dot);
    expect(container.querySelectorAll('td')).toHaveLength(cells);
  } finally {style.remove();}
});

it('formats every table value, axis and tooltip with two decimals and caps at 999.99 without a prefix', async () => {
  vi.stubGlobal('PointerEvent', MouseEvent);
  mocks.query.mockResolvedValue({...snapshot,peers:[{...snapshot.peers[0],samples:[{at_ms:59000,rtt_ms:1500},{at_ms:60000,rtt_ms:1500}],current_ms:0.004,p50_ms:1.235,p90_ms:999.99,p99_ms:1000,max_ms:1000000,average_jitter_ms:0,max_jitter_ms:12345.678}]});
  const {container} = render(<LatencyChart active/>);
  await screen.findByRole('button',{name:'Show all devices in chart'});
  expect(screen.getAllByRole('cell').slice(1,-1).map(cell=>cell.textContent)).toEqual(['0.00 ms','1.24 ms','999.99 ms','0.00 ms']);
  const labels = Array.from(container.querySelectorAll('.lat__grid text'),text=>text.textContent!);
  expect(labels.every(label=>/^\d+\.\d{2} ms$/.test(label))).toBe(true);
  expect(labels[2]).toBe('999.99 ms');
  fireEvent.pointerMove(container.querySelector('.lat__plot')!,{clientX:470});
  expect(container.querySelector('.lat__tip-row b')?.textContent).toBe('999.99 ms');
  const style = document.createElement('style'); style.textContent = styles; document.head.append(style);
  try {
    for (const element of [container.querySelector('table'),container.querySelector('.lat__grid text'),container.querySelector('.lat__tip-row b')]) {
      expect(getComputedStyle(element!).fontVariantNumeric).toBe('tabular-nums');
    }
  } finally {style.remove();}
});

it('draws one continuous path for every measured second and keeps real missing samples as gaps', async () => {
  const samples = Array.from({length:60},(_,i)=>({at_ms:(i+1)*1000,rtt_ms:4+i/100}));
  mocks.query.mockResolvedValue({...snapshot,peers:[{...snapshot.peers[0],samples}]});
  let event!: (value:unknown)=>void;
  mocks.listen.mockImplementationOnce(async(handler:(value:unknown)=>void)=>{event=handler;return vi.fn();});
  const {container}=render(<LatencyChart active/>);
  await screen.findByRole('cell',{name:'Peer'});
  const path = () => container.querySelector('.lat__line')!.getAttribute('d')!;
  expect(path().match(/M/g)).toHaveLength(1);
  expect(path().match(/L/g)).toHaveLength(59);
  act(()=>event({type:'connection_quality',data:{...snapshot,revision:2,peers:[{...snapshot.peers[0],samples:samples.filter(s=>s.at_ms!==30_000)}]}}));
  expect(path().match(/M/g)).toHaveLength(2);
  act(()=>event({type:'connection_quality',data:{...snapshot,revision:3,peers:[{...snapshot.peers[0],samples:samples.map(s=>s.at_ms===30_000?{...s,rtt_ms:null}:s)}]}}));
  expect(path().match(/M/g)).toHaveLength(2);
});
